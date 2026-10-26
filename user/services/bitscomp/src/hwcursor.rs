//! WY-11: the pointer on the display engine's hardware cursor (GX-5a, FBIO_BITSOS_CURSOR) instead of in the frames.
//! A pointer move is one ioctl made right where the motion event is handled (`cursor_moved`), not at the next
//! frame: that is what removes the lag. The image is uploaded only when the shape changes (`cursor_sync`, every
//! loop turn and after each event: a named shape from the theme, or a client's cursor surface at its hotspot).
//! Software drawing (`cursor.rs`) stays the fallback: no hardware cursor (ENODEV, ENOTTY on Linux: for good), or
//! a shape the hardware refuses (EINVAL: bigger than 256, EPERM, EBUSY, EIO: for that shape); switching to software
//! hides the hardware cursor first, so there is never a double pointer. A refusal that may pass (EPERM, EBUSY, EIO,
//! a failed move) tries the hardware again after RETRY, doubling up to RETRY_MAX, so a passing hiccup does not
//! leave the pointer in the frames until the next shape change (WY-11 fixer).

use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::utils::{CommitCounter, with_renderer_surface_state};
use smithay::input::pointer::CursorIcon;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;

use crate::cursor_theme::{CursorImage, Themes};
use crate::state::App;

pub const ENODEV: i32 = libc::ENODEV;
pub const ENOTTY: i32 = libc::ENOTTY;
pub const EINVAL: i32 = libc::EINVAL;
/// The display engine's largest cursor (DCN 3.1.4 max_cursor_size; gopfb fbdev/cursor.rs).
pub const HW_MAX: u32 = 256;
const RETRY: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(16);

/// A hardware cursor door: `/dev/fb0`'s ioctl (`backend::fbcursor`) or the headless fake. Errors are errnos.
pub trait HwCursor {
    /// A new image, shown with its hotspot at `at` (screen pixels).
    fn image(&mut self, img: &CursorImage, at: (i32, i32)) -> Result<(), i32>;
    /// The hotspot to `at`.
    fn moved(&mut self, at: (i32, i32)) -> Result<(), i32>;
    fn hide(&mut self) -> Result<(), i32>;
    /// What `ctl cursor` adds (the fake's record).
    fn report(&self) -> String {
        String::new()
    }
    /// `ctl cursor fail ERRNO N`: the next N calls fail with ERRNO (the fake only; false elsewhere).
    fn inject(&mut self, _errno: i32, _n: u32) -> bool {
        false
    }
}

/// The shape the pointer should have now; a change uploads a new image.
#[derive(Clone, PartialEq, Debug)]
pub enum Key {
    Hidden,
    Named(CursorIcon, u32),
    /// The surface, its commit, the output scale's bits and the hotspot (a new set_cursor may move only that).
    Surface(ObjectId, CommitCounter, u64, (i32, i32)),
}

/// The pointer's image state (App::cursor).
pub struct Pointer {
    pub hw: Option<Box<dyn HwCursor>>,
    /// The hardware cursor shows the pointer (the frames carry none).
    pub on_hw: bool,
    hw_visible: bool,
    pub key: Option<Key>,
    /// The image of a named shape (also the software pointer's), None for a surface or hidden.
    pub img: Option<Rc<CursorImage>>,
    pub themes: Themes,
    pub theme: String,
    /// theme.cursor.size (logical px).
    pub size: u32,
    pub moves: u64,
    pub uploads: u64,
    pub refused: u64,
    /// Motion event handled -> move ioctl returned, the last and the largest (us).
    pub move_us: (u64, u64),
    /// The last upload's (what, w, h, hot).
    pub shown: String,
    /// When to try the hardware again after a refusal that may pass, and the wait after that one.
    retry_at: Option<Instant>,
    retry_wait: Duration,
    pub retries: u64,
}

impl Pointer {
    pub fn new(theme: &str, size: u32) -> Self {
        Pointer {
            hw: None,
            on_hw: false,
            hw_visible: false,
            key: None,
            img: None,
            themes: Themes::new(theme),
            theme: theme.to_string(),
            size,
            moves: 0,
            uploads: 0,
            refused: 0,
            move_us: (0, 0),
            shown: "-".into(),
            retry_at: None,
            retry_wait: RETRY,
            retries: 0,
        }
    }

    fn hide_hw(&mut self) {
        if self.hw_visible
            && let Some(hw) = self.hw.as_mut()
        {
            let _ = hw.hide();
        }
        self.hw_visible = false;
    }

    /// An error of the door: ENODEV/ENOTTY end the hardware cursor for good; an image the engine cannot take
    /// (EINVAL) stays in software for this shape; anything else (EPERM, EBUSY, EIO, a failed move) is retried.
    fn refused(&mut self, what: &str, e: i32) {
        self.refused += 1;
        self.hide_hw();
        self.on_hw = false;
        if e == ENODEV || e == ENOTTY {
            let last = self.hw.take().map(|h| h.report()).unwrap_or_default();
            let sep = if last.is_empty() { "" } else { ": " };
            eprintln!("bitscomp: cursor: no hardware cursor ({what}: errno {e}); the pointer is drawn into the frames{sep}{last}");
            self.retry_at = None;
            return;
        }
        let retry = e != EINVAL || what == "a move";
        if retry {
            self.retry_at = Some(Instant::now() + self.retry_wait);
            self.retry_wait = (self.retry_wait * 2).min(RETRY_MAX);
        }
        if self.refused <= 3 {
            let then = if retry { "tried again shortly" } else { "this shape is drawn into the frames" };
            eprintln!("bitscomp: cursor: hardware cursor refused {what} (errno {e}); drawn into the frames, {then}");
        }
    }

    /// The retry is due: forget the shape so the next sync uploads it again.
    fn retry_due(&mut self) -> bool {
        if self.on_hw || self.hw.is_none() || !self.retry_at.is_some_and(|t| Instant::now() >= t) {
            return false;
        }
        self.retry_at = None;
        self.retries += 1;
        self.key = None;
        true
    }

    /// bitscomp ends: the hardware cursor goes off while bitscomp still holds the screen (before the back end
    /// gives the console back), then the door closes.
    pub fn shutdown(&mut self) {
        if let Some(mut hw) = self.hw.take() {
            let r = hw.hide();
            self.hw_visible = false;
            let fake = hw.report();
            eprintln!("bitscomp: cursor: hidden at exit ({}){}{fake}", if r.is_ok() { "ok" } else { "refused" }, if fake.is_empty() { "" } else { ": " });
        }
    }

    /// `ctl cursor fail ERRNO N`.
    pub fn inject(&mut self, errno: i32, n: u32) -> bool {
        self.hw.as_mut().is_some_and(|h| h.inject(errno, n))
    }

    pub fn report(&self) -> String {
        let mode = if self.on_hw { "hw" } else { "software" };
        let fake = self.hw.as_ref().map(|h| h.report()).unwrap_or_default();
        format!(
            "mode={mode} hw={} shape={} theme={} size={} moves={} uploads={} refused={} retries={} move_us_last={} move_us_max={}{}{fake}",
            self.hw.is_some(),
            self.shown,
            self.theme,
            self.size,
            self.moves,
            self.uploads,
            self.refused,
            self.retries,
            self.move_us.0,
            self.move_us.1,
            if fake.is_empty() { "" } else { " " },
        )
    }
}

pub enum Want {
    Nothing,
    Image(Rc<CursorImage>),
    Software,
}

impl App {
    /// The pointer's pixel size for the theme: theme.cursor.size at the output scale.
    pub fn cursor_px(&self) -> u32 {
        (self.cursor.size as f64 * self.theme.scale).round().max(1.0) as u32
    }

    /// The pointer's hotspot in screen pixels.
    pub fn pointer_px(&self) -> (i32, i32) {
        let p = self.pointer.to_physical(self.theme.scale);
        (p.x.round() as i32, p.y.round() as i32)
    }

    fn cursor_key(&self) -> Key {
        let px = self.cursor_px();
        if let Some(d) = self.cursor_deco {
            return Key::Named(d, px);
        }
        if self.cursor_hidden {
            return Key::Hidden;
        }
        if let Some(s) = self.cursor_surface.as_ref().filter(|s| s.is_alive()) {
            let commit = with_renderer_surface_state(s, |st| st.current_commit()).unwrap_or_default();
            return Key::Surface(s.id(), commit, self.theme.scale.to_bits(), crate::cursor_surface::hotspot(s));
        }
        Key::Named(self.cursor_named.unwrap_or(CursorIcon::Default), px)
    }

    /// Brings the image up to date with the shape (bitscomp's own, the client's, hidden): cheap when unchanged.
    pub fn cursor_sync(&mut self) {
        if !self.draw_pointer {
            return;
        }
        if self.cursor.retry_due() {
            eprintln!("bitscomp: cursor: trying the hardware cursor again");
        }
        let key = self.cursor_key();
        if self.cursor.key.as_ref() == Some(&key) {
            return;
        }
        self.cursor.key = Some(key.clone());
        let want = match &key {
            Key::Hidden => Want::Nothing,
            Key::Named(icon, px) => Want::Image(self.cursor.themes.get(*icon, *px)),
            Key::Surface(..) => match self.cursor_surface.clone() {
                Some(s) => crate::cursor_surface::image(&s, self.theme.scale),
                None => Want::Nothing,
            },
        };
        self.cursor.img = match (&key, &want) {
            (Key::Named(..), Want::Image(i)) => Some(i.clone()),
            _ => None,
        };
        let was = self.cursor.on_hw;
        let at = self.pointer_px();
        let p = &mut self.cursor;
        match want {
            _ if p.hw.is_none() => p.on_hw = false,
            Want::Nothing => {
                p.hide_hw();
                p.on_hw = true;
                p.shown = "hidden".into();
            }
            Want::Software => {
                p.hide_hw();
                p.on_hw = false;
            }
            Want::Image(img) if img.w > HW_MAX || img.h > HW_MAX => p.refused("a big image", EINVAL),
            Want::Image(img) => match p.hw.as_mut().map(|h| h.image(&img, at)) {
                Some(Ok(())) => {
                    if !was && p.retry_wait > RETRY {
                        eprintln!("bitscomp: cursor: back on the hardware cursor");
                    }
                    p.on_hw = true;
                    p.hw_visible = true;
                    p.uploads += 1;
                    p.retry_wait = RETRY;
                    p.shown = format!("{} image={}x{} hot={},{}", img.what, img.w, img.h, img.hot.0, img.hot.1);
                }
                Some(Err(e)) => p.refused("an image", e),
                None => p.on_hw = false,
            },
        }
        if !self.cursor.on_hw {
            self.cursor.shown = match &self.cursor.img {
                Some(i) => format!("{} image={}x{} hot={},{}", i.what, i.w, i.h, i.hot.0, i.hot.1),
                None if key == Key::Hidden => "hidden".into(),
                None => "surface".into(),
            };
        }
        // The software pointer changes (or appears, or goes): a frame.
        if !self.cursor.on_hw || was != self.cursor.on_hw {
            self.needs_frame = true;
        }
    }

    /// A motion event was handled at `t_in`: the hardware cursor follows now (no frame).
    pub fn cursor_moved(&mut self, t_in: Instant) {
        if !self.cursor.on_hw || self.cursor.key == Some(Key::Hidden) {
            return;
        }
        let at = self.pointer_px();
        let Some(r) = self.cursor.hw.as_mut().map(|h| h.moved(at)) else { return };
        match r {
            Ok(()) => {
                let us = t_in.elapsed().as_micros() as u64;
                let p = &mut self.cursor;
                p.moves += 1;
                p.hw_visible = true;
                p.move_us = (us, p.move_us.1.max(us));
            }
            Err(e) => {
                // Drawn in software for now (its image, or the surface, is already at hand); the retry, or the
                // next shape, tries the hardware again.
                self.cursor.refused("a move", e);
                self.needs_frame = true;
            }
        }
    }
}

/// The headless back end's stand-in (`--pointer fake`): records what a hardware cursor would be told.
#[derive(Default)]
pub struct Fake {
    pub images: u64,
    pub moves: u64,
    pub hides: u64,
    pub at: (i32, i32),
    pub visible: bool,
    pub last: String,
    pub last_move: Option<Instant>,
    /// `ctl cursor fail`: the next `fail.1` calls answer errno `fail.0`.
    pub fail: (i32, u32),
}

impl Fake {
    fn failing(&mut self) -> Result<(), i32> {
        if self.fail.1 == 0 {
            return Ok(());
        }
        self.fail.1 -= 1;
        Err(self.fail.0)
    }
}

impl HwCursor for Fake {
    fn image(&mut self, img: &CursorImage, at: (i32, i32)) -> Result<(), i32> {
        // The engine's checks (gopfb fbdev/cursor.rs of gx-GX-5a-cr.patch: size, hotspot inside, pixel count).
        let bad = img.w == 0 || img.h == 0 || img.w > HW_MAX || img.h > HW_MAX || img.hot.0 >= img.w || img.hot.1 >= img.h;
        if bad || img.px.len() != (img.w * img.h) as usize {
            return Err(EINVAL);
        }
        self.failing()?;
        self.images += 1;
        self.at = at;
        self.visible = true;
        self.last = format!("{}x{}+{},{}", img.w, img.h, img.hot.0, img.hot.1);
        Ok(())
    }
    fn moved(&mut self, at: (i32, i32)) -> Result<(), i32> {
        self.failing()?;
        self.moves += 1;
        self.at = at;
        self.visible = true;
        self.last_move = Some(Instant::now());
        Ok(())
    }
    fn hide(&mut self) -> Result<(), i32> {
        self.hides += 1;
        self.visible = false;
        Ok(())
    }
    fn report(&self) -> String {
        format!(
            "fake_images={} fake_moves={} fake_hides={} fake_at={},{} fake_visible={} fake_image={}",
            self.images, self.moves, self.hides, self.at.0, self.at.1, self.visible, self.last
        )
    }
    fn inject(&mut self, errno: i32, n: u32) -> bool {
        self.fail = (errno, n);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_refuses_what_the_engine_refuses() {
        let mut f = Fake::default();
        let big = CursorImage { w: 300, h: 10, hot: (0, 0), px: vec![0; 3000], what: "x".into() };
        assert_eq!(f.image(&big, (0, 0)), Err(EINVAL));
        let ok = crate::cursor_theme::arrow_image(1);
        assert_eq!(f.image(&ok, (5, 6)), Ok(()));
        assert_eq!((f.images, f.at, f.visible, f.last.as_str()), (1, (5, 6), true, "12x19+0,0"));
    }

    #[test]
    fn refusal_kinds() {
        let mut p = Pointer::new("Adwaita", 32);
        p.hw = Some(Box::new(Fake::default()));
        p.on_hw = true;
        p.refused("an image", libc::EBUSY);
        assert!(p.hw.is_some() && !p.on_hw);
        p.refused("an image", ENODEV);
        assert!(p.hw.is_none());
        assert!(p.report().starts_with("mode=software hw=false"));
    }

    #[test]
    fn a_passing_refusal_is_retried_with_a_growing_wait() {
        let mut p = Pointer::new("Adwaita", 32);
        p.hw = Some(Box::new(Fake::default()));
        p.key = Some(Key::Hidden);
        p.refused("an image", libc::EBUSY);
        assert!(!p.retry_due(), "not before the wait");
        assert_eq!(p.retry_wait, RETRY * 2);
        p.retry_at = Some(Instant::now());
        assert!(p.retry_due() && p.key.is_none() && p.retries == 1);
        p.refused("a move", EINVAL);
        assert!(p.retry_at.is_some() && p.retry_wait == RETRY * 4);
        for _ in 0..8 {
            p.refused("a move", libc::EIO);
        }
        assert_eq!(p.retry_wait, RETRY_MAX);
        // An image the engine cannot take is not retried; ENODEV ends it all.
        p.retry_at = None;
        p.refused("a big image", EINVAL);
        assert!(p.retry_at.is_none());
        p.refused("an image", ENOTTY);
        assert!(p.hw.is_none() && !p.retry_due());
    }

    #[test]
    fn fake_failure_injection_and_hotspot_check() {
        let mut f = Fake::default();
        let ok = crate::cursor_theme::arrow_image(1);
        assert!(f.inject(libc::EBUSY, 2));
        assert_eq!(f.image(&ok, (0, 0)), Err(libc::EBUSY));
        assert_eq!(f.moved((1, 1)), Err(libc::EBUSY));
        assert_eq!(f.moved((1, 1)), Ok(()));
        let bad_hot = CursorImage { hot: (12, 0), ..crate::cursor_theme::arrow_image(1) };
        assert_eq!(f.image(&bad_hot, (0, 0)), Err(EINVAL));
    }

    #[test]
    fn shutdown_hides_the_cursor() {
        let mut p = Pointer::new("Adwaita", 32);
        p.hw = Some(Box::new(Fake::default()));
        p.shutdown();
        assert!(p.hw.is_none() && !p.hw_visible);
    }
}
