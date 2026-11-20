//! DB3, EINS-B: the popup's look (boards 1 and 2): the head with the ring, "Durchblick", what it sees and the key;
//! the input line in an accent frame (or the asked question), the hint under it, the tools' lines in mono, the
//! answer or the step, Ja and Nein with "Enter = Ja · Esc = Nein", and the footer with the level pill, the cloud
//! line (a yellow dot) and "Esc schließt"; and a frame in the accent colour around every element the answer or the
//! step names. Drawn over everything but the installer's question; one cached image for the popup and one per frame.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use crate::assist::ToolState;
use crate::assist_level as lv;
use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::Px;
use crate::db_app::Stage;
use crate::db_layout::{self as lay, Button, Content, Geo};
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::theme::{Theme, alpha};

pub const PLACEHOLDER: &str = "Frag etwas oder sag, was ich tun soll …";
/// Board 1: under the input line before the first question.
pub const HINT: &str = "Enter ohne Text: „Erklär mir, was ich hier sehe.“ Er kann hier dasselbe wie in der Seitenleiste.";
pub const KEYS: &str = "Enter = Ja · Esc = Nein";
/// The cloud's yellow (board 1's #e3bd5f).
const CLOUD: [f32; 4] = [0.89, 0.741, 0.373, 1.0];

#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub geo: Geo,
    pub sees: String,
    pub key: String,
    pub field: String,
    pub before: String,
    pub editable: bool,
    pub focus: Option<Button>,
    pub field_focused: bool,
    pub hint: Option<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    pub keys: Option<String>,
    pub pill: String,
    pub cloud: Option<String>,
    pub scroll: usize,
    pub big: bool,
    pub theme: u64,
}

#[derive(Default)]
pub struct Images {
    panel: Option<(Look, MemoryRenderBuffer)>,
    frames: Vec<([i32; 4], Rect, (i32, i32), MemoryRenderBuffer)>,
}

impl App {
    /// What the pill says: what it sees ("sieht: Brave · Suchseite"), or that it sees nothing of this program.
    pub(crate) fn db_sees(&self) -> String {
        match self.db.snapshot.as_ref() {
            Some(w) if self.db.hidden => format!("sieht nicht: {}", w.app),
            Some(w) if w.title.is_empty() => format!("sieht: {}", w.app),
            Some(w) => format!("sieht: {} · {}", w.app, w.title),
            None => "sieht kein Fenster".into(),
        }
    }

    /// The popup's body now (the answer, a step, a question of a tool, why it cannot answer).
    pub(crate) fn db_texts(&self) -> String {
        let d = &self.db;
        match &d.stage {
            Stage::Setup(m) | Stage::Failed(m) => m.clone(),
            Stage::Input if d.hidden => format!("{}.", lv::NOT_SEEN),
            Stage::Input => String::new(),
            Stage::Waiting if d.shown.trim().is_empty() => format!("{} denkt nach …", self.assist.name),
            Stage::Waiting | Stage::Answering | Stage::Done if d.shown.trim().is_empty() => "…".to_string(),
            Stage::Waiting | Stage::Answering | Stage::Done => d.shown.trim().to_string(),
            Stage::Step => match &d.asking {
                Some(a) => a.text.clone(),
                None => self.db_step_text(),
            },
            Stage::Acting => self.db_step_text(),
        }
    }

    /// The line above a step: "Schritt n", or "Löschen fragt immer" (board 5).
    fn db_title(&self) -> Option<String> {
        if self.db.stage != Stage::Step && self.db.stage != Stage::Acting {
            return None;
        }
        match &self.db.asking {
            Some(a) if a.name == crate::assist::DELETE_TOOL => Some(crate::assist::DELETE_TITLE.into()),
            Some(_) => Some("Der Assistent fragt".into()),
            None => self.db_step_title(),
        }
    }

    /// The buttons now (the on-screen keyboard only in the sofa mode), the footer's included.
    pub fn db_buttons(&self) -> Vec<Button> {
        let mut v: Vec<Button> = self.db.buttons().into_iter().filter(|b| *b != Button::Keyboard || self.sofa.active).collect();
        if self.db.level_open {
            v.extend(lv::Level::ALL.map(Button::LevelPick));
        } else {
            v.push(Button::Level);
        }
        if !v.contains(&Button::Close) {
            v.push(Button::Close);
        }
        v
    }

    /// The cloud line: whenever a provider outside this machine answers (`ai.cloud_notice`, board 1).
    pub(crate) fn assist_cloud_line(&self, short: bool) -> Option<String> {
        let s = self.assist.status.as_ref()?;
        let cloud = s.place.as_deref() == Some("cloud");
        let shown = self.settings.flag("ai.cloud_notice").unwrap_or(true);
        (cloud && shown).then(|| if short { format!("{} (Cloud)", self.assist.name) } else { format!("geht an {} (Cloud)", self.assist.name) })
    }

    fn db_tool_lines(&self) -> Vec<String> {
        self.db.tools.iter().map(|(name, line, state)| {
            let what = if line.is_empty() { name.clone() } else { line.clone() };
            match state {
                ToolState::Runs | ToolState::Asks | ToolState::Done => what,
                s => format!("{what} ({})", s.word()),
            }
        }).collect()
    }

    /// The panel's parts, placed at the top of the screen.
    pub fn db_geo(&self) -> Option<Geo> {
        if !self.db.open || self.db_sofa_typing() {
            return None;
        }
        let t = &self.theme;
        let s = t.scale;
        let body = self.db_texts();
        let buttons = self.db_buttons();
        let big = self.sofa.active;
        let screen = self.area_full();
        let top = { let c = self.chrome().top; if c.h > 0 && c.y <= screen.y { c.y + c.h } else { screen.y + 12 } };
        let tools = self.db_tool_lines();
        let title = self.db_title();
        let hint = (self.db.stage == Stage::Input && !self.db.hidden).then_some(HINT);
        let cloud = self.assist_cloud_line(self.db.stage == Stage::Step);
        let pill = format!("{}  ▾", self.assist_level().name());
        let keys = (self.db.stage == Stage::Step).then_some(KEYS);
        let c = Content {
            hint,
            tools: &tools,
            title: title.as_deref(),
            body: &body,
            note: self.db.note.as_deref().or_else(|| self.db_step_why_asked().filter(|_| self.db.stage == Stage::Step && self.db.asking.is_none())),
            buttons: &buttons,
            keys,
            pill: &pill,
            levels: self.db.level_open,
            cloud: cloud.as_deref(),
            big,
            max_h: lay::max_height(screen, top),
        };
        let lg = |r: [i32; 4]| Rect { x: (r[0] as f64 / s) as i32, y: (r[1] as f64 / s) as i32, w: (r[2] as f64 / s).ceil() as i32, h: (r[3] as f64 / s).ceil() as i32 };
        let mut avoid: Vec<Rect> = self.db_frames().into_iter().map(lg).collect();
        if let Some(e) = self.db.pointed.as_ref().and_then(|p| self.db.snapshot.as_ref()?.find(p)) {
            avoid.push(lg(e.rect));
        }
        Some(lay::geo(t, &c, |w, h| lay::place(screen, top, &avoid, w, h)))
    }

    fn db_look(&self) -> Option<Look> {
        let geo = self.db_geo()?;
        let editable = self.db.has_field();
        let field = if editable { self.db.field.text.clone() } else { self.db.question.clone() };
        let key = crate::db_settings::Taste::of(&self.settings).name().to_string();
        Some(Look {
            sees: self.db_sees(),
            key,
            before: if editable { self.db.field.before().to_string() } else { String::new() },
            field,
            editable,
            focus: self.db.focus,
            field_focused: self.db.focus.is_none() && editable,
            hint: (self.db.stage == Stage::Input && !self.db.hidden).then(|| HINT.to_string()),
            title: self.db_title(),
            note: self.db_note_shown(),
            keys: (self.db.stage == Stage::Step).then(|| KEYS.to_string()),
            pill: format!("{}  ▾", self.assist_level().name()),
            cloud: self.assist_cloud_line(self.db.stage == Stage::Step),
            scroll: self.db.scroll,
            big: self.sofa.active,
            theme: self.theme_gen,
            geo,
        })
    }
}

fn draw(l: &Look, t: &Theme) -> Canvas {
    let g = &l.geo;
    let p = Px::new(g.panel, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let one = p.line();
    let rad = p.v(28.0);
    let panel = p.b(g.panel);
    c.rrect(panel, rad, t.panel);
    c.rrect_line(panel, rad, one, t.line_strong);
    let (sf, mf, btf, bf) = (lay::small_font(t, l.big), lay::mono_font(t, l.big), lay::button_font(t, l.big), lay::body_font(t, l.big));
    // The head: the ring, the name, what it sees, the key.
    let hd = p.r(g.head);
    let cy = hd.y as f32 + hd.h as f32 / 2.0;
    let ring_x = hd.x as f32 + p.v(13.0);
    c.ring(ring_x, cy, p.v(11.0), p.v(2.4), t.accent);
    let name = lay::title_font(t, l.big);
    let nx = (ring_x + p.v(22.0)) as i32;
    let nw = c.text_fit(&name, Rect { x: nx, y: hd.y, w: hd.w / 3, h: hd.h }, "Durchblick", t.text);
    let mono_w = mf.width(&l.sees).min(hd.w / 2);
    let chip = Rect { x: nx + nw + p.v(12.0) as i32, y: hd.y + hd.h / 2 - p.v(12.0) as i32, w: mono_w + p.v(24.0) as i32, h: p.v(24.0) as i32 };
    c.rrect(Box2::of(chip), chip.h as f32 / 2.0, t.bg);
    c.text_fit(&mf, Rect { x: chip.x + p.v(12.0) as i32, w: chip.w - p.v(24.0) as i32, ..chip }, &l.sees, t.muted);
    c.text_right(&mf, hd, &l.key, t.muted);
    // The input line (or the asked question) in its frame.
    let fr = p.r(g.field);
    let ff = lay::field_font(t, l.big);
    c.rrect(Box2::of(fr), p.v(14.0), t.bg);
    if l.editable {
        let ring = Box2::of(fr).inset(-p.v(4.0));
        c.rrect_line(ring, p.v(18.0), p.v(2.0), if l.field_focused { t.accent } else { t.line });
    }
    let inner = Rect { x: fr.x + p.v(18.0) as i32, w: fr.w - p.v(36.0) as i32, ..fr };
    let enter_w = mf.width("Enter");
    let text_band = Rect { w: inner.w - enter_w - p.v(12.0) as i32, ..inner };
    if l.editable && l.field.is_empty() {
        c.text_fit(&ff, text_band, PLACEHOLDER, t.muted);
    } else {
        c.text_fit(&ff, text_band, &l.field, if l.editable { t.text } else { t.muted });
    }
    if l.field_focused {
        let x = text_band.x + ff.width(&l.before).min(text_band.w);
        c.fill(x, fr.y + fr.h / 4, p.v(2.0).max(1.0) as i32, fr.h / 2, t.accent);
    }
    if l.editable {
        c.text_right(&mf, inner, "Enter", t.muted);
    }
    if let (Some(r), Some(hint)) = (g.hint, l.hint.as_ref()) {
        c.text_fit(&sf, p.r(r), hint, t.muted);
    }
    if let Some(r) = g.tools {
        let r = p.r(r);
        let n = g.tool_lines.len().max(1) as i32;
        let lh = r.h / n;
        for (i, line) in g.tool_lines.iter().enumerate() {
            let band = Rect { x: r.x + p.v(12.0) as i32, y: r.y + i as i32 * lh, w: r.w - p.v(12.0) as i32, h: lh };
            c.fill(r.x, band.y + lh / 5, p.v(3.0) as i32, lh * 3 / 5, t.accent);
            c.text_fit(&mf, band, line, t.muted);
        }
    }
    if let (Some(r), Some(title)) = (g.title, l.title.as_ref()) {
        c.text_fit(&sf, p.r(r), title, t.accent);
    }
    // The text; a cut one says which lines show (the arrows, LB/RB on the pad scroll it).
    let b = p.r(g.body);
    let (n, rows) = (g.lines.len(), g.body_rows.max(1));
    if n > 0 {
        let line_h = b.h / rows as i32;
        let cut = n > rows;
        let shown = if cut { rows - 1 } else { rows };
        let first = if cut { l.scroll.min(n - shown) } else { 0 };
        for (i, line) in g.lines.iter().skip(first).take(shown).enumerate() {
            c.text_fit(&bf, Rect { x: b.x, y: b.y + i as i32 * line_h, w: b.w, h: line_h }, line, t.text);
        }
        if cut {
            let keys = if l.big { "LB/RB" } else { "Pfeiltasten" };
            let more = format!("{}–{} von {} Zeilen ({keys})", first + 1, first + shown, n);
            c.text_right(&sf, Rect { x: b.x, y: b.y + shown as i32 * line_h, w: b.w, h: line_h }, &more, t.muted);
        }
    }
    if let (Some(nr), Some(n)) = (g.note, l.note.as_ref()) {
        c.text_fit(&sf, p.r(nr), n, t.muted);
    }
    if let (Some(kr), Some(k)) = (g.keys, l.keys.as_ref()) {
        c.text_fit(&sf, p.r(kr), k, t.muted);
    }
    for (bt, br) in &g.buttons {
        let r = Box2::of(p.r(*br));
        let focused = l.focus == Some(*bt);
        match bt {
            Button::Close => c.text_right(&sf, p.r(*br), bt.label(), if focused { t.accent } else { t.muted }),
            Button::Level => {
                c.rrect_line(r, r.h / 2.0, if focused { p.v(2.5) } else { one }, if focused { t.accent } else { t.line_strong });
                c.text_mid(&btf, p.r(*br), &l.pill, t.text);
            }
            Button::LevelPick(_) => {
                let on = l.pill.starts_with(bt.label());
                c.rrect(r, r.h / 2.0, if on { t.accent_soft } else { t.panel2 });
                c.rrect_line(r, r.h / 2.0, if focused { p.v(2.5) } else { one }, if focused || on { t.accent } else { t.line });
                c.text_mid(&btf, p.r(*br), bt.label(), t.text);
            }
            _ => {
                let strong = matches!(bt, Button::StepYes | Button::OpenKi);
                c.rrect(r, r.h / 2.0, if strong { t.accent } else { t.panel2 });
                c.rrect_line(r, r.h / 2.0, if focused { p.v(2.5) } else { one }, if focused { t.text } else { t.line });
                c.text_mid(&btf, p.r(*br), bt.label(), if strong { t.on_accent } else { t.text });
            }
        }
    }
    if let (Some(cr), Some(cl)) = (g.cloud, l.cloud.as_ref()) {
        let r = p.r(cr);
        let dot = p.v(5.0);
        c.disc(r.x as f32 + dot, r.y as f32 + r.h as f32 / 2.0, dot, CLOUD);
        c.text_fit(&sf, Rect { x: r.x + p.v(16.0) as i32, w: (r.w - p.v(16.0) as i32).max(1), ..r }, cl, t.muted);
    }
    c
}

/// A frame around `rect` (physical px): its place (logical), its size in pixels, its image.
fn frame(rect: [i32; 4], t: &Theme) -> (Rect, (i32, i32), Canvas) {
    let m = (6.0 * t.scale) as i32;
    let (w, h) = (rect[2] + 2 * m, rect[3] + 2 * m);
    let mut c = Canvas::new(w, h);
    let b = Box2::of(Rect { x: 0, y: 0, w, h });
    c.rrect_line(b, 8.0 * t.scale as f32, (3.0 * t.scale as f32).max(2.0), t.accent);
    c.rrect_line(b.inset((3.0 * t.scale as f32).max(2.0)), 6.0 * t.scale as f32, 1.0, alpha(t.accent, 0.35));
    let s = t.scale;
    let at = Rect { x: ((rect[0] - m) as f64 / s).floor() as i32, y: ((rect[1] - m) as f64 / s).floor() as i32, w: (w as f64 / s).ceil() as i32, h: (h as f64 / s).ceil() as i32 };
    (at, (w, h), c)
}

/// The popup's elements, topmost first: the bar, the panel, then the frames; none while it is closed.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let mut out = Vec::new();
    // The picture of the window comes from this frame: the panel waits for it.
    if app.db.want_shot.is_some() {
        return out;
    }
    // The assistant's bar over everything of the popup's (also while it is closed and a sidebar chat acts).
    out.extend(crate::db_act_draw::elements(app, renderer));
    let t = app.theme.clone();
    let frames = app.db_frames();
    if let Some(l) = app.db_look() {
        let im = &mut app.db.images;
        if im.panel.as_ref().is_none_or(|(o, _)| *o != l) {
            im.panel = Some((l.clone(), draw(&l, &t).into_buffer()));
        }
        let p = crate::header::px_rect(l.geo.panel, t.scale);
        if let Some((_, buf)) = &im.panel {
            out.extend(crate::deco::image(renderer, buf, l.geo.panel, (p.w, p.h), t.scale));
        }
    } else {
        app.db.images.panel = None;
    }
    let im = &mut app.db.images;
    if im.frames.iter().map(|f| f.0).ne(frames.iter().copied()) {
        im.frames = frames
            .iter()
            .map(|r| {
                let (at, size, c) = frame(*r, &t);
                (*r, at, size, c.into_buffer())
            })
            .collect();
    }
    for (_, at, size, buf) in &im.frames {
        out.extend(crate::deco::image(renderer, buf, *at, *size, t.scale));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(t: &Theme, c: &Content) -> Look {
        let geo = lay::geo(t, c, |w, h| Rect { x: 0, y: 0, w, h });
        Look {
            geo,
            sees: "sieht: Brave · Suchseite".into(),
            key: "Win+F1".into(),
            field: String::new(),
            before: String::new(),
            editable: true,
            focus: None,
            field_focused: true,
            hint: Some(HINT.into()),
            title: None,
            note: None,
            keys: None,
            pill: "Fragt vorher  ▾".into(),
            cloud: Some("geht an Claude (Cloud)".into()),
            scroll: 0,
            big: false,
            theme: 0,
        }
    }

    #[test]
    fn the_popup_and_a_frame_are_drawn_in_both_modes() {
        for dark in [true, false] {
            let mut t = Theme::default();
            t.dark = dark;
            let buttons = [Button::StepYes, Button::StepNo, Button::Level, Button::Close];
            let c = Content { hint: Some(HINT), tools: &[], title: Some("Schritt 1"), body: "Ich schreibe „Georg“ in die Adresszeile.", note: None, buttons: &buttons, keys: Some(KEYS), pill: "Fragt vorher  ▾", levels: false, cloud: Some("Claude (Cloud)"), big: false, max_h: 1000 };
            let l = look(&t, &c);
            let cv = draw(&l, &t);
            let p = Px::new(l.geo.panel, t.scale);
            let yes = p.r(l.geo.buttons.iter().find(|(b, _)| *b == Button::StepYes).unwrap().1);
            assert_eq!(cv.pixel(yes.x + 3, yes.y + yes.h / 2), crate::canvas::pack(t.accent), "Ja is the strong button");
            let cl = p.r(l.geo.cloud.unwrap());
            assert_ne!(cv.pixel(cl.x + p.v(5.0) as i32, cl.y + cl.h / 2), cv.pixel(cl.x + cl.w - 2, cl.y + 1), "the yellow dot");
            let (_, (w, h), f) = frame([100, 100, 50, 20], &t);
            assert_ne!(f.pixel(1, h / 2), 0, "the frame's line");
            assert_eq!(f.pixel(w / 2, h / 2), 0, "the element itself stays visible");
        }
    }

    #[test]
    fn every_text_has_its_glyphs() {
        let t = Theme::default();
        let f = lay::body_font(&t, false);
        for s in [PLACEHOLDER, HINT, KEYS, crate::db_app::REAL_INPUT, crate::db_act_app::AUTO_WOULD, lv::NOT_SEEN, "sieht: Brave · Suchseite", crate::assist::DELETE_TITLE] {
            assert!(s.chars().all(|ch| ch == ' ' || f.draws(ch)), "{s}");
        }
        let b = lay::button_font(&t, false);
        for bt in Button::ALL {
            assert!(bt.label().chars().all(|ch| ch == ' ' || b.draws(ch)), "{}", bt.label());
        }
        assert!("▾●".chars().all(|ch| b.draws(ch)), "the pill's arrow");
    }
}
