//! DB5b, EINS-B: the bar at the top of the screen while the Automodus runs a task's steps without asking (also
//! over the sofa mode, and while the acting chat is the sidebar's): what it does now and that Esc / B stops it.
//! It is in DB1's list (window `desktop`) so the desk and agents see it; acting on it is never allowed
//! (`db_act_own`).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use uilist::{Action, Element, Role};

use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::Px;
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Theme, alpha};

pub const BAR: &str = "Der Assistent handelt";

#[derive(Default)]
pub struct BarImage(Option<(String, Rect, u64, MemoryRenderBuffer)>);

fn font(t: &Theme, big: bool) -> crate::text::Font {
    t.font(Family::Ui, if big { 22.0 } else { 16.0 }, Weight::SemiBold)
}

impl App {
    /// The bar's text and place (logical px), while it shows.
    pub fn db_bar(&self) -> Option<(String, Rect)> {
        if !self.db_auto() {
            return None;
        }
        let n = self.act.task.as_ref().map_or(0, |t| t.steps);
        let keys = if self.sofa.active { "B hält an" } else { "Esc hält an" };
        let text = format!("{BAR} · Schritt {n} · {keys}");
        let t = &self.theme;
        let big = self.sofa.active;
        let screen = self.area_full();
        let w = (font(t, big).width(&text) as f64 / t.scale).ceil() as i32 + if big { 80 } else { 60 };
        let h = if big { 56 } else { 42 };
        let w = w.min(screen.w - 16).max(1);
        Some((text, Rect { x: screen.x + (screen.w - w) / 2, y: screen.y + 8, w, h }))
    }

    pub(crate) fn ui_durchblick_bar(&self) -> Option<Element> {
        let (text, r) = self.db_bar()?;
        let px = self.ui_px(r);
        let stop = Element::new(Role::Button, "Anhalten", px).key("durchblick-bar:stop").acts(&[Action::Press]);
        Some(Element::new(Role::Group, BAR, px).key("durchblick-bar").value(text).child(stop))
    }
}

fn draw(text: &str, r: Rect, t: &Theme, big: bool) -> Canvas {
    let p = Px::new(r, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let b = Box2::of(crate::layout::Rect { x: 0, y: 0, w, h });
    c.rrect(b, b.h / 2.0, t.panel);
    c.rrect(b, b.h / 2.0, alpha(t.accent, 0.28));
    c.rrect_line(b, b.h / 2.0, p.v(2.5), t.accent);
    let cy = h as f32 / 2.0;
    let dot = p.v(if big { 24.0 } else { 18.0 });
    c.disc(dot, cy, p.v(if big { 7.0 } else { 5.5 }), t.accent);
    let tx = (dot + p.v(16.0)) as i32;
    c.text_fit(&font(t, big), crate::layout::Rect { x: tx, y: 0, w: (w - tx - p.v(12.0) as i32).max(1), h }, text, t.text);
    c
}

/// The bar's element, drawn above the panel; none while it does not show.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let Some((text, r)) = app.db_bar() else {
        app.db.bar = BarImage::default();
        return Vec::new();
    };
    let t = app.theme.clone();
    let big = app.sofa.active;
    let theme = app.theme_gen;
    if app.db.bar.0.as_ref().is_none_or(|(s, o, g, _)| *s != text || *o != r || *g != theme) {
        app.db.bar = BarImage(Some((text.clone(), r, theme, draw(&text, r, &t, big).into_buffer())));
    }
    let p = crate::header::px_rect(r, t.scale);
    match &app.db.bar.0 {
        Some((_, _, _, buf)) => crate::deco::image(renderer, buf, r, (p.w, p.h), t.scale).into_iter().collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_is_drawn_in_both_modes() {
        let r = Rect { x: 0, y: 0, w: 420, h: 42 };
        for t in [Theme::default(), Theme { scale: 2.0, ..Theme::default() }] {
            let c = draw("Der Assistent handelt · Schritt 1 · Esc hält an", r, &t, false);
            let (w, h) = Px::new(r, t.scale).size();
            assert_ne!(c.pixel(w / 2, h / 2), 0, "drawn");
            assert_eq!(c.pixel(0, 0) >> 24, 0, "round ends");
        }
    }
}
