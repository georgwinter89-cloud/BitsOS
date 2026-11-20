//! DB3: the picture of a window Durchblick has no elements for (a game, a program without a tree). Taken from
//! the frame on the screen when the panel opens (before the panel is drawn over anything), kept in memory only,
//! and sent only after Georg said yes to it (or the program's Automodus is on, or the model is on this machine).
//! A PNG, made smaller in whole steps until its longer side is at most `SIDE_MAX` (what providers look at anyway).

use smithay::backend::renderer::element::Element;
use smithay::utils::Scale;

use crate::layout::Rect;
use crate::render::Composer;
use crate::state::App;

const SIDE_MAX: i32 = 1568;
/// The preview's small copy.
pub const THUMB_W: i32 = 320;

#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub w: i32,
    pub h: i32,
    pub png: Vec<u8>,
    /// XRGB rows of the small copy for the preview.
    pub thumb: Vec<u32>,
    pub thumb_w: i32,
    pub thumb_h: i32,
}

/// The pixels of `r` (physical px) out of a frame, every `step`-th one.
fn crop(px: &[u32], stride: usize, size: (i32, i32), r: Rect, step: i32) -> (Vec<u32>, i32, i32) {
    let x0 = r.x.clamp(0, size.0);
    let y0 = r.y.clamp(0, size.1);
    let x1 = (r.x + r.w).clamp(0, size.0);
    let y1 = (r.y + r.h).clamp(0, size.1);
    let step = step.max(1);
    let (w, h) = ((x1 - x0 + step - 1) / step, (y1 - y0 + step - 1) / step);
    let mut out = Vec::with_capacity((w * h).max(0) as usize);
    for y in (y0..y1).step_by(step as usize) {
        for x in (x0..x1).step_by(step as usize) {
            out.push(px.get(y as usize * stride + x as usize).copied().unwrap_or(0));
        }
    }
    (out, w.max(0), h.max(0))
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut c = kind.to_vec();
    c.extend_from_slice(data);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crate::icon_png::crc32(&c).to_be_bytes());
}

/// XRGB pixels as an 8-bit RGB PNG.
pub fn png(px: &[u32], w: i32, h: i32) -> Vec<u8> {
    let mut rows = Vec::with_capacity((w * h * 3 + h) as usize);
    for y in 0..h as usize {
        rows.push(0);
        for &p in &px[y * w as usize..(y + 1) * w as usize] {
            rows.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The pixels of a picture before it is a PNG (the copy is quick; the PNG is made on a thread).
pub struct Raw {
    big: Vec<u32>,
    w: i32,
    h: i32,
    thumb: Vec<u32>,
    thumb_w: i32,
    thumb_h: i32,
}

pub fn raw(px: &[u32], stride: usize, size: (i32, i32), r: Rect) -> Option<Raw> {
    if r.w <= 0 || r.h <= 0 {
        return None;
    }
    let step = (r.w.max(r.h) + SIDE_MAX - 1) / SIDE_MAX;
    let (big, w, h) = crop(px, stride, size, r, step);
    if w == 0 || h == 0 {
        return None;
    }
    let (thumb, thumb_w, thumb_h) = crop(px, stride, size, r, ((r.w + THUMB_W - 1) / THUMB_W).max(1));
    Some(Raw { big, w, h, thumb, thumb_w, thumb_h })
}

impl Raw {
    pub fn shot(self) -> Shot {
        Shot { png: png(&self.big, self.w, self.h), w: self.w, h: self.h, thumb: self.thumb, thumb_w: self.thumb_w, thumb_h: self.thumb_h }
    }
}

/// The picture of `r` from a frame.
#[cfg(test)]
pub fn of_frame(px: &[u32], stride: usize, size: (i32, i32), r: Rect) -> Option<Shot> {
    raw(px, stride, size, r).map(Raw::shot)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Whether any of `above` (physical px) lies over `r`.
pub fn covers(above: impl IntoIterator<Item = Rect>, r: Rect) -> bool {
    above.into_iter().any(|a| a.w > 0 && a.h > 0 && a.x < r.x + r.w && r.x < a.x + a.w && a.y < r.y + r.h && r.y < a.y + a.h)
}

/// The frame's build, at a window's own frame (or veil), with everything drawn above it (topmost first, the
/// pointer left out): whether something lies over the window whose picture Durchblick waits for. The picture is
/// cut from the screen's frame, so a notice, a menu or another window over it would be in it too; the window's
/// own frame (its rounded corners lie over the content) is not something else.
pub fn cover<E: Element>(app: &mut App, win: &crate::win::Window, above: &[E], scale: Scale<f64>) {
    let Some(r) = app.db.want_shot else { return };
    if app.db.shot_cover.is_some() || crate::ui_reg::wid(win) != app.db.target {
        return;
    }
    let rects = above.iter().map(|e| {
        let g = e.geometry(scale);
        Rect { x: g.loc.x, y: g.loc.y, w: g.size.w, h: g.size.h }
    });
    app.db.shot_cover = Some(covers(rects, r));
}

/// The end of the frame's build: a window that was not drawn at all counts as covered; the next turn takes it.
pub fn frame_done(app: &mut App) {
    if app.db.want_shot.is_some() {
        app.db.shot_cover.get_or_insert(true);
        app.needs_frame = true;
    }
}

/// A frame drawn after the panel asked (and without the panel) is waited for, at most this long.
const FRAME_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Takes the pictures asked for (the main loop's turn, after the control socket's): the popup's of its window
/// from the first frame drawn after the ask, and the desktop tools' (EINS-B) once a frame was drawn after theirs.
pub fn take(app: &mut App, comp: &mut dyn Composer) {
    take_desk(app, comp);
    let Some(r) = app.db.want_shot else { return };
    if app.db.shot_cover.is_none() && app.db.shot_asked.is_some_and(|t| t.elapsed() < FRAME_WAIT) {
        return;
    }
    app.db.want_shot = None;
    app.db.shot_covered = app.db.shot_cover != Some(false);
    comp.sync_pixels();
    let (px, stride) = comp.pixels();
    let picture = raw(px, stride, comp.size(), r);
    let (generation, Some(tx)) = (app.db.generation, app.db.tx.clone()) else { return };
    let made = std::thread::Builder::new().name("bitscomp-db-png".into()).spawn(move || {
        let shot = picture.map(Raw::shot);
        eprintln!("bitscomp: durchblick: picture of the window taken ({} bytes, kept in memory only)", shot.as_ref().map_or(0, |s| s.png.len()));
        let _ = tx.send((generation, crate::db_worker::Msg::Shot(shot)));
    });
    if made.is_err() {
        app.db_shot_taken();
    }
}

/// The desktop tools' pictures (`foto`, `sehen` with `bild`): each PNG is made on a thread, sent back for its call.
fn take_desk(app: &mut App, comp: &mut dyn Composer) {
    let due = |s: &crate::assist_route::Shot| !s.taken && (!app.needs_frame || s.asked.elapsed() >= crate::assist_route::SHOT_WAIT);
    if !app.assist_route.shots.iter().any(due) {
        return;
    }
    let Some(tx) = app.db.tx.clone() else { return };
    comp.sync_pixels();
    let (px, stride) = comp.pixels();
    let size = comp.size();
    for s in app.assist_route.shots.iter_mut().filter(|s| !s.taken) {
        // Taken once: the call waits for its PNG now, never for a second one.
        s.taken = true;
        let (call, picture, tx) = (s.call.clone(), raw(px, stride, size, s.rect), tx.clone());
        let _ = std::thread::Builder::new().name("bitscomp-desk-png".into()).spawn(move || {
            let _ = tx.send((crate::db_app::ACT_GEN, crate::db_worker::Msg::DeskShot(call, picture.map(Raw::shot))));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn something_over_the_window_counts_only_where_it_lies_over_it() {
        let r = Rect { x: 100, y: 100, w: 400, h: 300 };
        let at = |x, y, w, h| Rect { x, y, w, h };
        assert!(!covers([], r));
        assert!(!covers([at(0, 0, 100, 100), at(500, 100, 50, 50), at(100, 400, 400, 10), at(200, 200, 0, 50)], r), "beside it, or empty");
        assert!(covers([at(450, 350, 100, 100)], r), "a corner over it");
        assert!(covers([at(0, 0, 2000, 1000)], r), "over all of it");
    }

    #[test]
    fn base64_as_everyone_writes_it() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_big_window_becomes_a_smaller_png_the_decoder_reads_back() {
        // The library's decoder reads up to 1024 px: a frame that halves to fit it.
        let size = (2000, 1000);
        let px: Vec<u32> = (0..size.0 * size.1).map(|i| if i % size.0 < 1000 { 0x00ff_0000 } else { 0x0000_00ff }).collect();
        let s = of_frame(&px, size.0 as usize, size, Rect { x: 0, y: 0, w: 2000, h: 1000 }).unwrap();
        assert_eq!((s.w, s.h), (1000, 500), "made smaller in whole steps (every second pixel)");
        let img = crate::icon_png::decode(&s.png).expect("a valid PNG");
        assert_eq!((img.w, img.h), (1000, 500));
        assert!(s.thumb_w <= THUMB_W && s.thumb_w > 0 && s.thumb.len() == (s.thumb_w * s.thumb_h) as usize);
        assert!(of_frame(&px, size.0 as usize, size, Rect { x: 10, y: 10, w: 0, h: 5 }).is_none());
    }
}
