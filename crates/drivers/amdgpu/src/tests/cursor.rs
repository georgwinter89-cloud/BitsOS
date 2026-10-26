//! GX-5a against the fake engine: the register sequences of a cursor image, a move, a partly and a fully
//! off-screen position, a hide, the follow after a mode switch, the Quiesce restore, the refusals, and the cursor
//! lock released on every path.

use super::fake::*;
use super::flip::{attach_of, timing, Engine, Er, FakeClock};
use crate::cursor::{self, Alpha, CursorError, Place, Shape};
use crate::flip::FlipError;
use crate::layout as l;
use crate::scanout::Scanout;
use crate::tools_cursor::{self, CursorArgs};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use gopfb::{GopFb, Layout, RamTarget};

const W: u32 = 1024;
const H: u32 = 768;
/// 48 MHz reference over a 65 MHz pixel clock, 2^19 fixed point.
const REF_TO_PIX: u32 = 387_166;
/// The cursor BO's offset in graphics memory (any free one; the live code picks it).
const CUR_GPU: u64 = GPU + 64 * MB;

struct Rig {
    e: Arc<Engine>,
    s: Scanout<Er, FakeClock>,
    t: Arc<RamTarget>,
}

fn image() -> Image {
    let mut m = mini_pc();
    m.insert(l::hubp(0, l::REF_FREQ_TO_PIX_FREQ), REF_TO_PIX);
    m.insert(l::hubp(0, l::HUBP_VIEWPORT), H << 16 | W);
    m.insert(l::dpp(0, l::RECOUT_SIZE), H << 16 | W);
    m.insert(l::mpcc(0, l::MPCC_OPP_ID), 0);
    m
}

fn rig_with(img: Image, with_buf: bool) -> Rig {
    let a = attach_of(&img);
    let e = Engine::new(img);
    let fb = || GopFb::new(Arc::new(RamTarget::new(BYTES as usize, 0)), W, H, W, Layout::Bgrx, "test").unwrap();
    let mut s = Scanout::new(Er(e.clone()), FakeClock(e.clone()), &a, fb(), fb(), timing()).unwrap();
    let t = Arc::new(RamTarget::new(cursor::BYTES as usize, 0xEE));
    if with_buf {
        s.set_cursor_buffer(cursor::Buf::new(t.clone(), CUR_GPU).unwrap());
    }
    Rig { e, s, t }
}

fn rig() -> Rig {
    rig_with(image(), true)
}

/// The writes since `from`.
fn since(r: &Rig, from: usize) -> Vec<(u32, u32)> {
    r.e.writes()[from..].to_vec()
}

fn hub(reg: u32) -> u32 {
    l::hubp(0, reg)
}
fn dpp(reg: u32) -> u32 {
    l::dpp(0, reg)
}

fn shape64() -> Shape {
    Shape::new(64, 64, 0, 0, Alpha::Premultiplied).unwrap()
}

fn dst(x: u32) -> u32 {
    (u64::from(x) * u64::from(REF_TO_PIX) >> 19) as u32
}

/// Sets a 64x64 image with its tip on (100, 50); returns the write count after it.
fn with_image(r: &Rig) -> usize {
    r.s.cursor_image(shape64(), &vec![0xFF11_2233; 64 * 64], (100, 50)).unwrap();
    r.e.writes().len()
}

#[test]
fn image_writes_attributes_then_position_inside_the_cursor_lock() {
    let r = rig();
    let n = r.e.writes().len();
    with_image(&r);
    let lpc8 = 3 << 24;
    assert_eq!(
        since(&r, n),
        vec![
            (l::cur_lock(0), 1),
            (hub(l::CUR_ADDR_HI), (CUR_GPU >> 32) as u32),
            (hub(l::CUR_ADDR), CUR_GPU as u32),
            (hub(l::CUR_SIZE), 64 << 16 | 64),
            (hub(l::CUR_CONTROL), 2 << 8 | lpc8),
            (hub(l::CUR_SETTINGS), 0x300),
            (dpp(l::CNVC_CUR_CONTROL), 2 << 4),
            (dpp(l::CNVC_CUR_FP_SCALE_BIAS), 0x3C00),
            (hub(l::CUR_POSITION), 100 << 16 | 50),
            (hub(l::CUR_HOT_SPOT), 0),
            (hub(l::CUR_DST_OFFSET), dst(100)),
            (hub(l::CUR_CONTROL), 2 << 8 | lpc8 | 1),
            (dpp(l::CNVC_CUR_CONTROL), 2 << 4 | 1),
            (l::cur_lock(0), 0),
        ]
    );
    // The first image goes into surface 0: the pixels, then transparent up to the pitch.
    let mut b = [0u8; 4];
    assert!(r.t.read(0, &mut b));
    assert_eq!(u32::from_le_bytes(b), 0xFF11_2233);
    let s = r.s.cursor_state();
    assert_eq!((s.1, s.2), ((100, 50), true));
}

#[test]
fn image_is_padded_to_its_pitch_and_alternates_surfaces() {
    let r = rig();
    let sh = Shape::new(40, 20, 3, 4, Alpha::Straight).unwrap();
    assert_eq!(sh.pitch(), 64);
    r.s.cursor_image(sh, &vec![0x8080_8080; 40 * 20], (10, 10)).unwrap();
    let px = |off: usize| {
        let mut b = [0u8; 4];
        assert!(r.t.read(off, &mut b));
        u32::from_le_bytes(b)
    };
    assert_eq!(px(39 * 4), 0x8080_8080);
    assert_eq!(px(40 * 4), 0, "right of the image: transparent");
    assert_eq!(px((20 * 64) * 4), 0, "below the image: transparent");
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_MODE, 3 << 8, "straight alpha = mode 3");
    let n = r.e.writes().len();
    r.s.cursor_image(shape64(), &vec![1; 64 * 64], (10, 10)).unwrap();
    let second = CUR_GPU + cursor::SLOT_BYTES;
    assert!(since(&r, n).contains(&(hub(l::CUR_ADDR), second as u32)));
    assert!(px(cursor::SLOT_BYTES as usize) == 1, "the second image goes into surface 1");
    r.s.cursor_image(shape64(), &vec![2; 64 * 64], (10, 10)).unwrap();
    assert_eq!(r.e.reg(hub(l::CUR_ADDR)), CUR_GPU as u32, "and the third into surface 0 again");
}

#[test]
fn move_writes_only_what_changed() {
    let r = rig();
    let n = with_image(&r);
    r.s.cursor_move((300, 200)).unwrap();
    assert_eq!(
        since(&r, n),
        vec![(l::cur_lock(0), 1), (hub(l::CUR_POSITION), 300 << 16 | 200), (hub(l::CUR_DST_OFFSET), dst(300)), (l::cur_lock(0), 0)]
    );
    let n = r.e.writes().len();
    r.s.cursor_move((300, 200)).unwrap();
    assert_eq!(since(&r, n), vec![], "the same place writes nothing");
}

#[test]
fn partly_off_screen_moves_into_the_hotspot() {
    let r = rig();
    let n = with_image(&r);
    r.s.cursor_move((-10, -20)).unwrap();
    assert_eq!(
        since(&r, n),
        vec![
            (l::cur_lock(0), 1),
            (hub(l::CUR_POSITION), 0),
            (hub(l::CUR_HOT_SPOT), 10 << 16 | 20),
            (hub(l::CUR_DST_OFFSET), 0),
            (l::cur_lock(0), 0),
        ]
    );
    // The right and bottom edges: the hardware clips; still on.
    r.s.cursor_move((W as i32 - 1, H as i32 - 1)).unwrap();
    assert_eq!(r.e.reg(hub(l::CUR_POSITION)), (W - 1) << 16 | (H - 1));
    assert_eq!(r.e.reg(hub(l::CUR_HOT_SPOT)), 0);
    assert_ne!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0);
}

#[test]
fn fully_off_screen_disables_and_coming_back_writes_all() {
    let r = rig();
    let n = with_image(&r);
    r.s.cursor_move((-64, 10)).unwrap();
    let c = r.e.reg(hub(l::CUR_CONTROL));
    assert_eq!(
        since(&r, n),
        vec![(l::cur_lock(0), 1), (hub(l::CUR_CONTROL), c), (dpp(l::CNVC_CUR_CONTROL), 2 << 4), (l::cur_lock(0), 0)]
    );
    assert_eq!(c & l::CUR_ENABLE, 0);
    for off in [(W as i32, 0), (0, H as i32), (5, -64)] {
        r.s.cursor_move(off).unwrap();
        assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0, "{off:?}");
    }
    let n = r.e.writes().len();
    r.s.cursor_move((7, 8)).unwrap();
    let w: Vec<u32> = since(&r, n).iter().map(|w| w.0).collect();
    assert_eq!(
        w,
        vec![l::cur_lock(0), hub(l::CUR_POSITION), hub(l::CUR_HOT_SPOT), hub(l::CUR_DST_OFFSET), hub(l::CUR_CONTROL), dpp(l::CNVC_CUR_CONTROL), l::cur_lock(0)]
    );
    assert_eq!(r.e.reg(dpp(l::CNVC_CUR_CONTROL)) & l::CUR0_ENABLE, 1);
}

#[test]
fn hide_once_and_a_move_shows_again() {
    let r = rig();
    let n = with_image(&r);
    r.s.cursor_hide().unwrap();
    let w: Vec<u32> = since(&r, n).iter().map(|w| w.0).collect();
    assert_eq!(w, vec![l::cur_lock(0), hub(l::CUR_CONTROL), dpp(l::CNVC_CUR_CONTROL), l::cur_lock(0)]);
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0);
    let n = r.e.writes().len();
    r.s.cursor_hide().unwrap();
    assert_eq!(since(&r, n), vec![], "hidden already");
    r.s.cursor_move((100, 50)).unwrap();
    assert_ne!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0);
    assert!(r.s.cursor_state().2);
}

#[test]
fn mode_switch_size_clips_the_cursor() {
    let r = rig();
    with_image(&r);
    r.s.cursor_move((900, 100)).unwrap();
    // A switch to 800x600 (the fake keeps the registers; set_size is what the switch calls after it landed).
    r.s.set_size(800, 600);
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0, "past the new right edge");
    assert_eq!(r.s.cursor_state().3, Some(Place::OFF));
    r.s.set_size(W, H);
    assert_ne!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0, "the way back shows it again");
    assert_eq!(r.e.reg(hub(l::CUR_POSITION)), 900 << 16 | 100);
}

#[test]
fn restore_turns_the_cursor_off_and_releases_the_lock() {
    let r = rig();
    let n = r.e.writes().len();
    r.s.restore().unwrap();
    assert_eq!(since(&r, n), vec![], "no cursor used: nothing written");
    with_image(&r);
    r.e.st().regs.insert(l::cur_lock(0), 1);
    let n = r.e.writes().len();
    r.s.restore().unwrap();
    let w = since(&r, n);
    assert_eq!(w[..3].iter().map(|w| w.0).collect::<Vec<_>>(), vec![hub(l::CUR_CONTROL), dpp(l::CNVC_CUR_CONTROL), l::cur_lock(0)]);
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0);
    assert_eq!(r.e.reg(dpp(l::CNVC_CUR_CONTROL)) & l::CUR0_ENABLE, 0);
    assert_eq!(r.e.reg(l::cur_lock(0)), 0);
}

#[test]
fn a_failed_write_still_unlocks() {
    let r = rig();
    let n = with_image(&r);
    r.e.st().refuse = Some(hub(l::CUR_POSITION));
    let e = r.s.cursor_move((500, 500)).unwrap_err();
    assert_eq!(e, CursorError::Reg(FlipError::Write(hub(l::CUR_POSITION))));
    assert_eq!(since(&r, n).last(), Some(&(l::cur_lock(0), 0)));
    // The state forgot what the registers hold: the next move writes all of it.
    r.e.st().refuse = None;
    let n = r.e.writes().len();
    r.s.cursor_move((500, 500)).unwrap();
    assert_eq!(since(&r, n).len(), 7);
}

#[test]
fn refusals() {
    assert!(matches!(Shape::new(0, 8, 0, 0, Alpha::Premultiplied), Err(CursorError::Invalid(_))));
    assert!(matches!(Shape::new(257, 8, 0, 0, Alpha::Premultiplied), Err(CursorError::Invalid(_))));
    assert!(matches!(Shape::new(8, 8, 8, 0, Alpha::Premultiplied), Err(CursorError::Invalid(_))));
    assert!(Shape::new(256, 256, 255, 255, Alpha::Premultiplied).is_ok());
    let r = rig();
    let n = r.e.writes().len();
    assert!(matches!(r.s.cursor_move((1, 1)), Err(CursorError::Invalid(_))), "a move before any image");
    assert!(matches!(r.s.cursor_image(shape64(), &[0; 10], (0, 0)), Err(CursorError::Invalid(_))));
    assert_eq!(since(&r, n), vec![]);
    let nb = rig_with(image(), false);
    assert!(matches!(nb.s.cursor_image(shape64(), &vec![0; 64 * 64], (0, 0)), Err(CursorError::NoDevice(_))));
    assert!(matches!(nb.s.cursor_hide(), Err(CursorError::NoDevice(_))));
    let mut img = image();
    img.insert(l::mpcc(0, l::MPCC_OPP_ID), 0xF);
    let no = rig_with(img, true);
    assert!(matches!(no.s.cursor_image(shape64(), &vec![0; 64 * 64], (0, 0)), Err(CursorError::NoDevice(_))));
}

#[test]
fn a_cursor_found_on_is_turned_off() {
    let mut img = image();
    img.insert(hub(l::CUR_CONTROL), 0x0302_0201);
    let r = rig_with(img, true);
    assert_eq!(r.s.cursor_clear_found(), Ok(true));
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)), 0x0302_0200);
    assert_eq!(rig().s.cursor_clear_found(), Ok(false));
}

#[test]
fn placement_and_tables() {
    let sh = Shape::new(32, 32, 10, 12, Alpha::Premultiplied).unwrap();
    // Tip (5, 5) with hotspot (10, 12): top-left (-5, -7): position 0, hotspot (5, 7).
    let p = cursor::place((5, 5), &sh, (W, H), 1 << 19);
    assert_eq!(p, Place { enable: true, position: 0, hot_spot: 5 << 16 | 7, dst_offset: 0 });
    let p = cursor::place((110, 12), &sh, (W, H), 1 << 19);
    assert_eq!(p, Place { enable: true, position: 100 << 16, hot_spot: 0, dst_offset: 100 });
    // The pitch (64), not the image, decides "fully off".
    assert!(cursor::place((10 - 63, 0), &sh, (W, H), 0).enable);
    assert!(!cursor::place((10 - 64, 0), &sh, (W, H), 0).enable);
    assert!(!cursor::place((i32::MIN, i32::MAX), &sh, (W, H), 0).enable);
    assert_eq!(cursor::place((W as i32 + 9, 0), &sh, (W, H), 0).enable, true, "top-left x = W - 1");
    assert_eq!(cursor::place((5000, 0), &sh, (W, H), u64::MAX).dst_offset, 0, "off: nothing");
    assert_eq!(cursor::place((1000, 0), &sh, (W, H), u64::MAX).dst_offset, 0x1FFF, "clamped to 13 bits");
    assert_eq!([32, 33, 64, 65, 128, 129, 256].map(cursor::lines_per_chunk), [4, 3, 3, 2, 2, 1, 1]);
    assert_eq!(cursor::dst_factor(REF_TO_PIX, 2560, 1280), 2 * u64::from(REF_TO_PIX), "a 2x upscale doubles it");
    assert_eq!(cursor::dst_factor(REF_TO_PIX, 2560, 0), 0);
}

#[test]
fn tool_args_and_arrow() {
    let p = |s: &str| tools_cursor::cursor_args(&mcp::Json::parse(s).unwrap());
    assert_eq!(p("{}").unwrap(), CursorArgs::State);
    assert_eq!(p(r#"{"op":"show","x":-5,"y":7}"#).unwrap(), CursorArgs::Show { size: 64, x: -5, y: 7 });
    assert_eq!(p(r#"{"op":"move","x":1}"#).unwrap(), CursorArgs::Move { x: 1, y: 0 });
    assert_eq!(p(r#"{"op":"sweep"}"#).unwrap(), CursorArgs::Sweep { frames: 240 });
    assert!(p(r#"{"op":"show","size":65}"#).is_err());
    assert!(p(r#"{"op":"sweep","frames":3601}"#).is_err());
    assert!(p(r#"{"op":"move","x":5000000000}"#).is_err());
    assert!(p(r#"{"op":"jump"}"#).is_err());
    let a = tools_cursor::arrow(64);
    assert_eq!(a.len(), 64 * 64);
    assert_eq!(a[0], 0xFF00_0000, "the tip (hotspot 0,0) is the black edge");
    assert_eq!(a[30 * 64 + 5], 0xFFFF_FFFF, "inside: white");
    assert_eq!(a[63], 0, "outside: transparent");
    let m = tools_cursor::sweep_marks(W, H, 64);
    assert_eq!(m[0], (0, 0));
    assert_eq!(m[4], (-32, H as i32 / 2));
}

/// Reads one pixel of the cursor BO.
fn bo_px(r: &Rig, off: usize) -> u32 {
    let mut b = [0u8; 4];
    assert!(r.t.read(off, &mut b));
    u32::from_le_bytes(b)
}

#[test]
fn larger_pitches_program_size_pitch_and_fit_both_surfaces() {
    // 256 x 256 fills a whole surface; two of them fill the BO without overlapping.
    assert_eq!(cursor::SLOT_BYTES, 256 * 256 * 4);
    assert_eq!(cursor::BYTES, 2 * cursor::SLOT_BYTES);
    let r = rig();
    for (w, code, lpc, fill) in [(100u32, 1u32, 2u32, 0x1111_1111u32), (256, 2, 1, 0x2222_2222), (200, 2, 1, 0x3333_3333)] {
        let pitch = if w <= 128 { 128 } else { 256 };
        let sh = Shape::new(w, w, 0, 0, Alpha::Premultiplied).unwrap();
        assert_eq!(sh.pitch(), pitch);
        let n = r.e.writes().len();
        r.s.cursor_image(sh, &vec![fill; (w * w) as usize], (10, 10)).unwrap();
        let w_ = since(&r, n);
        assert!(w_.contains(&(hub(l::CUR_SIZE), pitch << 16 | pitch)), "SIZE = pitch x pitch for {w}");
        let c = r.e.reg(hub(l::CUR_CONTROL));
        assert_eq!(c & l::CUR_PITCH, code << l::CUR_PITCH_SHIFT, "CURSOR_PITCH for {w}");
        assert_eq!(c & l::CUR_LPC, lpc << l::CUR_LPC_SHIFT, "LINES_PER_CHUNK for {w}");
        let addr = u64::from(r.e.reg(hub(l::CUR_ADDR)));
        let base = (addr - (CUR_GPU & 0xFFFF_FFFF)) as usize;
        assert!(base == 0 || base == cursor::SLOT_BYTES as usize);
        let last = base + ((pitch * pitch - 1) * 4) as usize;
        assert!(last + 4 <= cursor::BYTES as usize, "the surface lies inside the BO");
        assert_eq!(bo_px(&r, base + ((w - 1) * 4) as usize), fill);
        assert_eq!(bo_px(&r, base + ((w * pitch - 1) * 4) as usize), if w == pitch { fill } else { 0 });
        assert_eq!(bo_px(&r, last), if w == pitch { fill } else { 0 }, "padding is transparent");
    }
    // The alternate surface still holds the previous (256) image untouched by the last (200) one.
    let shown = (u64::from(r.e.reg(hub(l::CUR_ADDR))) - (CUR_GPU & 0xFFFF_FFFF)) as usize;
    let other = cursor::SLOT_BYTES as usize - shown;
    assert_eq!(bo_px(&r, other + (256 * 256 - 1) * 4), 0x2222_2222);
}

#[test]
fn a_pixel_slice_of_the_wrong_length_writes_nothing() {
    let r = rig();
    let n = r.e.writes().len();
    for len in [0usize, 64 * 64 - 1, 64 * 64 + 1, 256 * 256 * 4] {
        assert!(matches!(r.s.cursor_image(shape64(), &vec![7; len], (0, 0)), Err(CursorError::Invalid(_))), "len {len}");
    }
    assert_eq!(since(&r, n), vec![]);
    assert_eq!(bo_px(&r, 0), 0xEEEE_EEEE, "no pixel written either");
    assert_eq!(bo_px(&r, cursor::SLOT_BYTES as usize), 0xEEEE_EEEE);
}

#[test]
fn a_failed_image_turns_the_cursor_off_and_skips_the_surface_it_may_fetch() {
    let r = rig();
    with_image(&r);
    assert_eq!(r.e.reg(hub(l::CUR_ADDR)), CUR_GPU as u32, "the first image is in surface 0");
    // The second image fails after its address (surface 1) was written.
    r.e.st().refuse = Some(hub(l::CUR_SETTINGS));
    assert!(r.s.cursor_image(shape64(), &vec![5; 64 * 64], (10, 10)).is_err());
    r.e.st().refuse = None;
    assert_eq!(r.e.reg(hub(l::CUR_ADDR)), (CUR_GPU + cursor::SLOT_BYTES) as u32);
    assert_eq!(r.e.reg(hub(l::CUR_CONTROL)) & l::CUR_ENABLE, 0, "turned off");
    assert_eq!(r.e.reg(dpp(l::CNVC_CUR_CONTROL)) & l::CUR0_ENABLE, 0);
    assert_eq!(r.e.reg(l::cur_lock(0)), 0, "cursor lock released");
    let (shape, _, shown, _) = r.s.cursor_state();
    assert!(shape.is_none() && !shown, "the image is forgotten");
    assert!(matches!(r.s.cursor_move((20, 20)), Err(CursorError::Invalid(_))), "no move on a forgotten image");
    // The next image goes into surface 0, not into surface 1 that the registers point at.
    r.s.cursor_image(shape64(), &vec![6; 64 * 64], (10, 10)).unwrap();
    assert_eq!(r.e.reg(hub(l::CUR_ADDR)), CUR_GPU as u32);
    assert_eq!(bo_px(&r, 0), 6);
    assert_eq!(bo_px(&r, cursor::SLOT_BYTES as usize), 5, "surface 1 kept the failed image's pixels");
}

#[test]
fn a_move_after_a_switch_places_against_the_new_size() {
    // The size is read under the pair lock now; single-threaded this checks the move sees the stored size.
    let r = rig();
    with_image(&r);
    r.s.set_size(640, 480);
    r.s.cursor_move((700, 100)).unwrap();
    let (_, _, _, p) = r.s.cursor_state();
    assert!(!p.unwrap().enable, "x 700 is off a 640 wide plane");
}
