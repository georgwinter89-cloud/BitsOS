//! What a hung window looks like (plan/spec/desktop.md §5, WY-4; WY-5 look of Details.dc.html): its last frame
//! stays, a veil in the background colour fades the client's part, and in the middle a dashed ring with "!",
//! "antwortet nicht", "seit N Sekunden · die anderen Fenster laufen weiter", and the buttons "Beenden" (ends the
//! program) and "Warten" (hides the veil for `WAIT_FOR`; the tab keeps saying "antwortet nicht" and the window
//! still gets no input). Geometry in logical pixels, shared by drawing, hit testing and `ctl hot`.

use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Physical, Point, Scale};

use crate::canvas::{Box2, Canvas};
use crate::header::px_rect;
use crate::layout::{Placement, Rect};
use crate::render::CompElement;
use crate::state::App;
use crate::text::{Font, Weight};
use crate::theme::Theme;
use crate::zones::Part;

pub const SAYS: &str = "antwortet nicht";
pub const BUTTON: &str = "Beenden";
pub const WAIT: &str = "Warten";
pub const WAIT_FOR: Duration = Duration::from_secs(10);

/// "seit 6 Sekunden · die anderen Fenster laufen weiter".
pub fn since_text(secs: u64) -> String {
    let unit = if secs == 1 { "Sekunde" } else { "Sekunden" };
    format!("seit {secs} {unit} · die anderen Fenster laufen weiter")
}

/// The box in the middle of the client's part `body` (never larger than it).
pub fn hung_box(body: Rect, t: &Theme) -> Rect {
    let (w, h) = (t.hung_box_size.0.min(body.w), t.hung_box_size.1.min(body.h));
    Rect { x: body.x + (body.w - w) / 2, y: body.y + (body.h - h) / 2, w, h }
}

fn buttons(body: Rect, t: &Theme) -> (Rect, Rect) {
    let b = hung_box(body, t);
    let (w, h) = (t.kill_button_size.0.min((b.w - 10) / 2).max(1), t.kill_button_size.1.min(b.h / 3).max(1));
    let y = b.y + b.h - h;
    let mid = b.x + b.w / 2;
    (Rect { x: mid - 5 - w, y, w, h }, Rect { x: mid + 5, y, w, h })
}

/// "Beenden", left of the middle at the bottom of the box.
pub fn kill_button(body: Rect, t: &Theme) -> Rect {
    buttons(body, t).0
}

/// "Warten", right of the middle.
pub fn wait_button(body: Rect, t: &Theme) -> Rect {
    buttons(body, t).1
}

fn fonts(t: &Theme) -> (Font, Font, Font) {
    (
        Font::new(t.fs(20.0), Weight::SemiBold, t.scale),
        Font::new(t.fs(14.0), Weight::Regular, t.scale),
        Font::new(t.fs(15.0), Weight::Regular, t.scale),
    )
}

/// The box as an image at output resolution; `hover`: the part under the pointer.
fn draw(body: Rect, hover: Option<Part>, secs: u64, t: &Theme) -> Canvas {
    let s = t.scale;
    let v = |l: i32| (l as f64 * s) as f32;
    let b = hung_box(body, t);
    let origin = px_rect(b, s);
    let rel = |r: Rect| {
        let p = px_rect(r, s);
        Rect { x: p.x - origin.x, y: p.y - origin.y, ..p }
    };
    let (title, sub, btn) = fonts(t);
    let mut c = Canvas::new(origin.w, origin.h);
    let cx = c.w as f32 / 2.0;
    c.arc(cx, v(28), v(24), v(2), Some((v(6), v(6), 0.0)), t.muted);
    c.line((cx, v(16)), (cx, v(30)), v(3), t.text);
    c.line((cx, v(37)), (cx, v(39)), v(3), t.text);
    let cw = c.w;
    let band = |y: i32, h: i32| Rect { x: 0, w: cw, ..rel(Rect { y: b.y + y, h, ..b }) };
    c.text_mid(&title, band(64, 28), SAYS, t.text);
    c.text_mid(&sub, band(96, 22), &since_text(secs), t.muted);
    let (k, w) = (rel(kill_button(body, t)), rel(wait_button(body, t)));
    let rad = v(10);
    let line = (s as f32).max(1.0);
    let kb = Box2::of(k);
    c.rrect(kb, rad, if hover == Some(Part::Kill) { crate::theme::alpha(t.danger, 0.3) } else { t.danger_chip });
    c.rrect_line(kb, rad, line, t.danger);
    c.text_mid(&btn, k, BUTTON, t.text);
    let wb = Box2::of(w);
    if hover == Some(Part::Wait) {
        c.rrect(wb, rad, t.panel2);
    }
    c.rrect_line(wb, rad, line, t.line);
    c.text_mid(&btn, w, WAIT, t.text);
    c
}

/// Whether the veil of `w` is shown now (hung and not waited for).
pub fn shown(app: &App, w: &Window) -> bool {
    app.is_hung(w) && app.veil_wait.get(&crate::ctl::win_id(w)).is_none_or(|&until| crate::pacer::now() >= until)
}

/// How many drawn boxes are kept (a cache: one that is gone is simply drawn again).
const HUNG_IMG_KEEP: usize = 8;

/// The veil and the box of a hung window, topmost first (they go above the window's own elements).
pub fn elements(app: &mut App, pixman: &mut PixmanRenderer, p: &Placement<Window>) -> Vec<CompElement> {
    let Some(id) = p.win.toplevel().map(|t| t.wl_surface().id()) else { return Vec::new() };
    if !shown(app, &p.win) {
        return Vec::new();
    }
    let t = app.theme.clone();
    let s = t.scale;
    let body = app.content_rect(p.frame);
    let hover = app.hover.as_ref().filter(|(w, _)| *w == p.win).map(|(_, part)| *part);
    let secs = app.hung_since(&p.win).map_or(0, |t0| crate::pacer::now().saturating_sub(t0).as_secs());
    let b = hung_box(body, &t);
    let px = px_rect(b, s);
    let key = (hover, secs, px.w, px.h, app.theme_gen);
    let at = match app.hung_img.iter().position(|(k, _)| *k == key) {
        Some(i) => i,
        None => {
            // R1: a few sizes are in use at a time (hover or not, a handful of tile sizes); the oldest goes first.
            if app.hung_img.len() >= HUNG_IMG_KEEP {
                app.hung_img.remove(0);
            }
            app.hung_img.push((key, draw(body, hover, secs, &t).into_buffer()));
            app.hung_img.len() - 1
        }
    };
    let buf = &app.hung_img[at].1;
    let mut out: Vec<CompElement> = crate::deco::image(pixman, buf, b, (px.w, px.h), s).into_iter().collect();
    let veil = app.veil.entry(id).or_insert_with(|| SolidColorBuffer::new((1, 1), t.veil));
    veil.update((body.w, body.h), t.veil);
    let loc: Point<i32, Physical> = Point::from(((body.x as f64 * s).round() as i32, (body.y as f64 * s).round() as i32));
    out.push(SolidColorRenderElement::from_buffer(veil, loc, Scale::from(s), 1.0, Kind::Unspecified).into());
    out
}

impl App {
    /// "Warten" on `w`'s veil.
    pub fn veil_wait_for(&mut self, w: &Window) {
        self.veil_wait.insert(crate::ctl::win_id(w), crate::pacer::now() + WAIT_FOR);
        self.needs_frame = true;
    }

    /// The next time a veil changes by itself (its seconds count, or a "Warten" runs out).
    pub fn veil_due(&mut self, now: Duration) -> Option<Duration> {
        let hung: Vec<Window> =
            self.placements().into_iter().filter(|p| p.visible && self.is_hung(&p.win)).map(|p| p.win).collect();
        let before = self.veil_wait.len();
        self.veil_wait.retain(|id, until| now < *until && hung.iter().any(|w| crate::ctl::win_id(w) == *id));
        if self.veil_wait.len() != before {
            self.needs_frame = true;
        }
        let mut due: Option<Duration> = None;
        for w in &hung {
            let next = match self.veil_wait.get(&crate::ctl::win_id(w)) {
                Some(&until) => until,
                None => {
                    let t0 = self.hung_since(w).unwrap_or(now);
                    t0 + Duration::from_secs(now.saturating_sub(t0).as_secs() + 1)
                }
            };
            due = Some(due.map_or(next, |d| d.min(next)));
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_and_buttons_sit_inside_the_body() {
        let t = crate::theme::test_theme();
        for body in [Rect { x: 10, y: 40, w: 600, h: 400 }, Rect { x: 0, y: 0, w: 90, h: 30 }] {
            let b = hung_box(body, t);
            assert!(b.x >= body.x && b.y >= body.y && b.x + b.w <= body.x + body.w && b.y + b.h <= body.y + body.h);
            let (k, w) = (kill_button(body, t), wait_button(body, t));
            for r in [k, w] {
                assert!(r.x >= b.x && r.y >= b.y && r.x + r.w <= b.x + b.w && r.y + r.h <= b.y + b.h, "{r:?} in {b:?}");
            }
            assert!(k.x + k.w <= w.x, "Beenden left of Warten");
        }
    }

    #[test]
    fn texts_fit_the_box_and_the_buttons() {
        let t = crate::theme::test_theme();
        let (title, sub, btn) = fonts(t);
        let body = Rect { x: 0, y: 0, w: 800, h: 600 };
        assert!(title.width(SAYS) <= px_rect(hung_box(body, t), t.scale).w);
        assert!(sub.width(&since_text(59)) <= px_rect(hung_box(body, t), t.scale).w);
        assert!(btn.width(BUTTON) <= px_rect(kill_button(body, t), t.scale).w);
        assert!(btn.width(WAIT) <= px_rect(wait_button(body, t), t.scale).w);
        assert_eq!(since_text(1), "seit 1 Sekunde · die anderen Fenster laufen weiter");
    }
}
