//! WY-11: pointer images from an Xcursor theme (Adwaita ships in the image at /usr/share/icons/Adwaita/cursors,
//! copied from CachyOS's adwaita-cursors), read with the `xcursor` crate. A named shape (wp_cursor_shape_v1,
//! bitscomp's own resize shapes) is looked up by its CSS name, then its legacy X names (the theme's aliases); the
//! file's nominal size is the smallest that is at least the wanted pixel size, else its largest; only the first
//! frame of an animated shape is used. Without a theme the built-in arrow stands in.

use std::collections::HashMap;
use std::rc::Rc;

use smithay::input::pointer::CursorIcon;

/// The theme bitscomp draws with and names to its programs (shipped in the image, user/packaging/manifest.txt).
pub const THEME: &str = "Adwaita";

/// One pointer image: pre-multiplied ARGB8888, row by row, the hotspot inside it.
#[derive(Debug, PartialEq)]
pub struct CursorImage {
    pub w: u32,
    pub h: u32,
    pub hot: (u32, u32),
    pub px: Vec<u32>,
    /// What it shows, for `ctl cursor` (a shape name, "arrow", "surface").
    pub what: String,
}

/// The theme's images by (shape, pixel size), loaded once each (a miss gives the arrow, also remembered).
pub struct Themes {
    name: String,
    cache: HashMap<(CursorIcon, u32), Rc<CursorImage>>,
}

impl Themes {
    pub fn new(name: &str) -> Self {
        Themes { name: name.to_string(), cache: HashMap::new() }
    }

    /// `icon` at about `px` pixels: the theme's, else the built-in arrow (`k` = px / 24 rounded, at least 1).
    pub fn get(&mut self, icon: CursorIcon, px: u32) -> Rc<CursorImage> {
        let name = &self.name;
        self.cache
            .entry((icon, px))
            .or_insert_with(|| {
                Rc::new(load(name, icon, px).unwrap_or_else(|| {
                    eprintln!("bitscomp: cursor: {} not in theme {name} (built-in arrow instead)", icon.name());
                    arrow_image(((px + 12) / 24).max(1))
                }))
            })
            .clone()
    }
}

/// The theme's file for `icon` (its CSS name, then the legacy names) parsed at nominal size for `px`.
pub fn load(theme: &str, icon: CursorIcon, px: u32) -> Option<CursorImage> {
    let t = xcursor::CursorTheme::load(theme);
    let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
    let path = names.into_iter().find_map(|n| t.load_icon(n))?;
    let data = std::fs::read(&path).ok()?;
    let images = xcursor::parser::parse_xcursor(&data)?;
    let size = nominal(images.iter().map(|i| i.size), px)?;
    let i = images.iter().find(|i| i.size == size)?;
    // The file's pixels are little-endian ARGB words, pre-multiplied (the Xcursor format); bad sizes are refused.
    let n = (i.width as usize).checked_mul(i.height as usize)?;
    if n == 0 || i.pixels_rgba.len() != n * 4 || i.xhot >= i.width || i.yhot >= i.height {
        return None;
    }
    let px = i.pixels_rgba.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    Some(CursorImage { w: i.width, h: i.height, hot: (i.xhot, i.yhot), px, what: icon.name().to_string() })
}

/// The nominal size to use for `px`: the smallest that is at least `px`, else the largest.
pub fn nominal(sizes: impl Iterator<Item = u32>, px: u32) -> Option<u32> {
    let v: Vec<u32> = sizes.collect();
    v.iter().copied().filter(|&s| s >= px).min().or_else(|| v.iter().copied().max())
}

/// The built-in arrow (`cursor::ARROW`) at `k` times its size, hotspot at the tip.
pub fn arrow_image(k: u32) -> CursorImage {
    let (w, h, bytes) = crate::cursor::arrow(k as i32, 0, 0);
    let px = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    CursorImage { w: w as u32, h: h as u32, hot: (0, 0), px, what: "arrow".into() }
}

/// `src` (w x h) scaled by `f` with the nearest pixel (a client's cursor buffer drawn at the output scale).
pub fn scale_nearest(src: &[u32], w: u32, h: u32, f: f64) -> (u32, u32, Vec<u32>) {
    let (nw, nh) = (((w as f64 * f).round() as u32).max(1), ((h as f64 * f).round() as u32).max(1));
    let mut out = Vec::with_capacity(nw as usize * nh as usize);
    for y in 0..nh {
        let sy = ((y as f64 + 0.5) / f).floor().min((h - 1) as f64) as u32;
        for x in 0..nw {
            let sx = ((x as f64 + 0.5) / f).floor().min((w - 1) as f64) as u32;
            out.push(src[(sy * w + sx) as usize]);
        }
    }
    (nw, nh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_size_is_the_next_larger_else_the_largest() {
        let s = [24, 30, 36, 48, 60, 72];
        assert_eq!(nominal(s.into_iter(), 40), Some(48));
        assert_eq!(nominal(s.into_iter(), 48), Some(48));
        assert_eq!(nominal(s.into_iter(), 20), Some(24));
        assert_eq!(nominal(s.into_iter(), 100), Some(72));
        assert_eq!(nominal([].into_iter(), 40), None);
    }

    #[test]
    fn arrow_hotspot_is_the_tip() {
        let a = arrow_image(2);
        assert_eq!((a.w, a.h, a.hot, a.px[0]), (24, 38, (0, 0), 0xff00_0000));
    }

    #[test]
    fn nearest_scaling() {
        let (w, h, p) = scale_nearest(&[1, 2, 3, 4], 2, 2, 2.0);
        assert_eq!((w, h, p), (4, 4, vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]));
        assert_eq!(scale_nearest(&[7], 1, 1, 0.5), (1, 1, vec![7]));
    }

    #[test]
    fn a_missing_theme_gives_the_arrow() {
        let mut t = Themes::new("no-such-theme-wy11");
        let a = t.get(CursorIcon::Text, 48);
        assert_eq!((a.what.as_str(), a.w), ("arrow", 24));
    }

    /// CachyOS's Adwaita, when the host has it (the image ships the same files).
    #[test]
    fn adwaita_text_at_48() {
        if !std::path::Path::new("/usr/share/icons/Adwaita/cursors/text").exists() {
            return;
        }
        let i = load("Adwaita", CursorIcon::Text, 40).expect("Adwaita text");
        assert_eq!((i.w, i.h, i.what.as_str()), (48, 48, "text"));
        assert!(i.hot.0 < 48 && i.hot.1 < 48);
        // A legacy-only name resolves through the aliases (CursorIcon::Default also has left_ptr).
        assert!(load("Adwaita", CursorIcon::Default, 24).is_some());
    }
}
