//! DB3, EINS-B: where the popup goes and what lies in it (boards 1 and 2). From the top of the screen, in the
//! middle, under the top bar (at the bottom instead when it would cover what it frames): its head (the name, what
//! it sees, the key), the input line (or the asked question), the hint under it, the tools' lines, the text (the
//! answer or the step), the buttons, and the footer with the level pill, the cloud line and "Esc schließt". Rects
//! in logical px; the text is wrapped with the font it is drawn in.

use crate::assist_level::Level;
use crate::layout::Rect;
use crate::text::{Family, Font, Weight};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// "Esc schließt" in the footer, and the set-up case's second button.
    Close,
    OpenKi,
    Keyboard,
    StepYes,
    StepNo,
    StepStop,
    /// The level pill ("Fragt vorher ▾"), and one of its three choices.
    Level,
    LevelPick(Level),
}

impl Button {
    pub fn label(self) -> &'static str {
        match self {
            Button::Close => "Esc schließt",
            Button::OpenKi => "Seite „KI“ öffnen",
            Button::Keyboard => "Tastatur",
            Button::StepYes => "Ja",
            Button::StepNo => "Nein",
            Button::StepStop => "Anhalten",
            Button::Level => "Stufe",
            Button::LevelPick(l) => l.name(),
        }
    }

    /// Its word in `ctl durchblick state` and `press`.
    pub fn word(self) -> &'static str {
        match self {
            Button::Close => "schliessen",
            Button::OpenKi => "ki",
            Button::Keyboard => "tastatur",
            Button::StepYes => "ja",
            Button::StepNo => "nein",
            Button::StepStop => "anhalten",
            Button::Level => "stufe",
            Button::LevelPick(Level::Answer) => "nur-antworten",
            Button::LevelPick(Level::Ask) => "fragt-vorher",
            Button::LevelPick(Level::Auto) => "automodus",
        }
    }

    pub const ALL: [Button; 10] = [
        Button::Close,
        Button::OpenKi,
        Button::Keyboard,
        Button::StepYes,
        Button::StepNo,
        Button::StepStop,
        Button::Level,
        Button::LevelPick(Level::Answer),
        Button::LevelPick(Level::Ask),
        Button::LevelPick(Level::Auto),
    ];

    pub fn of_word(w: &str) -> Option<Button> {
        Button::ALL.into_iter().find(|b| b.word() == w)
    }

    /// Only Georg's own hand presses it: the assistant acts in his place (a loosened level is judged by
    /// `assist_level_pick`, a tighter one is anyone's).
    pub fn consents(self) -> bool {
        matches!(self, Button::StepYes)
    }

    /// A button of the footer (drawn as text or a pill there, not in the buttons' row).
    pub fn in_footer(self) -> bool {
        matches!(self, Button::Close | Button::Level | Button::LevelPick(_))
    }
}

/// What the popup shows, as the layout needs it.
pub struct Content<'a> {
    pub hint: Option<&'a str>,
    pub tools: &'a [String],
    pub title: Option<&'a str>,
    pub body: &'a str,
    pub note: Option<&'a str>,
    pub buttons: &'a [Button],
    /// Beside the buttons: "Enter = Ja · Esc = Nein".
    pub keys: Option<&'a str>,
    /// The footer: the pill's text (or the three choices), the cloud line, whether "Esc schließt" shows.
    pub pill: &'a str,
    pub levels: bool,
    pub cloud: Option<&'a str>,
    pub big: bool,
    /// The tallest the panel may be (logical px): a longer answer scrolls, the field and buttons stay.
    pub max_h: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Geo {
    pub panel: Rect,
    pub head: Rect,
    pub field: Rect,
    pub hint: Option<Rect>,
    pub tools: Option<Rect>,
    pub tool_lines: Vec<String>,
    pub title: Option<Rect>,
    pub body: Rect,
    pub lines: Vec<String>,
    /// How many of `lines` show at once (fewer than all: the body scrolls, its last row says where).
    pub body_rows: usize,
    pub note: Option<Rect>,
    pub keys: Option<Rect>,
    pub buttons: Vec<(Button, Rect)>,
    pub cloud: Option<Rect>,
}

pub fn width(big: bool) -> i32 {
    if big { 900 } else { 760 }
}
const PAD: i32 = 24;
const GAP: i32 = 12;
const HEAD_H: i32 = 34;
const BUTTON_H: i32 = 36;
const FIELD_H: i32 = 54;
const TOOL_LINES: usize = 6;

pub fn body_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Ui, if big { 21.0 } else { 16.0 }, Weight::Regular)
}

pub fn field_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Ui, if big { 22.0 } else { 18.0 }, Weight::Regular)
}

pub fn small_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Ui, if big { 16.0 } else { 13.0 }, Weight::Regular)
}

pub fn mono_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Mono, if big { 15.0 } else { 12.0 }, Weight::Regular)
}

pub fn button_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Ui, if big { 18.0 } else { 13.0 }, Weight::Medium)
}

pub fn title_font(t: &Theme, big: bool) -> Font {
    t.font(Family::Display, if big { 26.0 } else { 22.0 }, Weight::SemiBold)
}

/// Lines of at most `max` physical px; a paragraph break stays one.
fn wrap(f: &Font, text: &str, max: i32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.trim().is_empty() {
            out.push(String::new());
        } else {
            out.extend(crate::chrome_draw::wrap(f, para, max.max(1)));
        }
    }
    out
}

/// A logical height from physical px.
fn lh(f: &Font, s: f64, factor: f64) -> i32 {
    ((f.height() as f64 * factor) / s).ceil() as i32
}

fn overlap(a: Rect, b: Rect) -> i64 {
    let w = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0) as i64;
    let h = ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0) as i64;
    w * h
}

/// The popup's place: from the top, in the middle, under `top` (the top bar's lower edge); at the bottom when it
/// would cover one of `avoid` (the framed and pointed elements) there and not there.
pub fn place(screen: Rect, top: i32, avoid: &[Rect], w: i32, h: i32) -> Rect {
    let w = w.min(screen.w - 2 * GAP).max(1);
    let h = h.min(screen.h - top.max(screen.y) - GAP).max(1);
    let x = screen.x + (screen.w - w) / 2;
    let up = Rect { x, y: top.max(screen.y), w, h };
    let down = Rect { x, y: screen.y + screen.h - GAP - h, w, h };
    let cost = |r: &Rect| avoid.iter().map(|a| overlap(*r, *a)).sum::<i64>();
    if cost(&up) > 0 && cost(&down) < cost(&up) { down } else { up }
}

/// The panel's tallest height on `screen` below `top`.
pub fn max_height(screen: Rect, top: i32) -> i32 {
    screen.y + screen.h - top.max(screen.y) - GAP
}

/// The panel's parts for `c`; `at` decides the place once the height is known. A body taller than the panel may
/// be gets fewer rows (one of them the "x–y von n" row), so the field, the buttons and the footer stay inside.
pub fn geo(t: &Theme, c: &Content, at: impl Fn(i32, i32) -> Rect) -> Geo {
    let full = stack(t, c, usize::MAX);
    let g = if full.panel.h > c.max_h && full.lines.len() > 1 {
        let line_h = full.body.h / full.lines.len() as i32;
        let cut = ((full.panel.h - c.max_h + line_h - 1) / line_h.max(1)) as usize + 1;
        stack(t, c, full.lines.len().saturating_sub(cut).max(2))
    } else {
        full
    };
    let panel = at(g.panel.w, g.panel.h);
    let shift = |r: Rect| Rect { x: r.x + panel.x, y: r.y + panel.y, ..r };
    Geo {
        panel,
        head: shift(g.head),
        field: shift(g.field),
        hint: g.hint.map(shift),
        tools: g.tools.map(shift),
        title: g.title.map(shift),
        body: shift(g.body),
        note: g.note.map(shift),
        keys: g.keys.map(shift),
        buttons: g.buttons.into_iter().map(|(b, r)| (b, shift(r))).collect(),
        cloud: g.cloud.map(shift),
        ..g
    }
}

/// The parts at (0, 0) with at most `rows` body rows.
fn stack(t: &Theme, c: &Content, rows: usize) -> Geo {
    let s = t.scale;
    let k = if c.big { 1.25 } else { 1.0 };
    let pad = (PAD as f64 * k) as i32;
    let w = width(c.big);
    let inner = w - 2 * pad;
    let px = |l: i32| (l as f64 * s) as i32;
    let (bf, sf, mf, btf) = (body_font(t, c.big), small_font(t, c.big), mono_font(t, c.big), button_font(t, c.big));
    let line_h = lh(&bf, s, 1.4);
    let small_h = lh(&sf, s, 1.35);
    let button_h = (BUTTON_H as f64 * k) as i32;
    let mut y = pad - 4;
    let head = Rect { x: pad, y, w: inner, h: (HEAD_H as f64 * k) as i32 };
    y += head.h + GAP;
    let field = Rect { x: pad, y, w: inner, h: (FIELD_H as f64 * k) as i32 };
    y += field.h + GAP;
    let hint = c.hint.map(|_| {
        let r = Rect { x: pad, y, w: inner, h: small_h };
        y += r.h + GAP;
        r
    });
    let mut tool_lines: Vec<String> = c.tools.iter().flat_map(|l| wrap(&mf, l, px(inner - 12))).collect();
    if tool_lines.len() > TOOL_LINES {
        tool_lines.drain(..tool_lines.len() - TOOL_LINES);
    }
    let mono_h = lh(&mf, s, 1.45);
    let tools = (!tool_lines.is_empty()).then(|| {
        let r = Rect { x: pad, y, w: inner, h: mono_h * tool_lines.len() as i32 };
        y += r.h + GAP;
        r
    });
    let title = c.title.map(|_| {
        let r = Rect { x: pad, y, w: inner, h: small_h };
        y += r.h + 4;
        r
    });
    let lines = if c.body.is_empty() { Vec::new() } else { wrap(&bf, c.body, px(inner)) };
    let body_rows = rows.min(lines.len());
    let body = Rect { x: pad, y, w: inner, h: line_h * body_rows as i32 };
    if body_rows > 0 {
        y += body.h + GAP;
    }
    let note = c.note.map(|_| {
        let r = Rect { x: pad, y, w: inner, h: small_h };
        y += r.h + GAP / 2;
        r
    });
    // The buttons in rows, as many as fit side by side; the keys' hint after them.
    let mut buttons = Vec::new();
    let (mut x, mut row_y) = (pad, y);
    for &b in c.buttons.iter().filter(|b| !b.in_footer()) {
        let bw = ((btf.width(b.label()) as f64 / s) as i32 + (40.0 * k) as i32).min(inner);
        if x > pad && x + bw > pad + inner {
            x = pad;
            row_y += button_h + GAP / 2;
        }
        buttons.push((b, Rect { x, y: row_y, w: bw, h: button_h }));
        x += bw + GAP;
    }
    let keys = c.keys.filter(|_| !buttons.is_empty()).map(|_| Rect { x, y: row_y, w: (pad + inner - x).max(1), h: button_h });
    if !buttons.is_empty() {
        y = row_y + button_h + GAP;
    }
    // The footer: the pill (or the three choices), the cloud line, "Esc schließt" on the right.
    let fy = y + 2;
    let pill_h = (30.0 * k) as i32;
    let mut fx = pad;
    let pills: Vec<Button> = if c.levels { Level::ALL.into_iter().map(Button::LevelPick).collect() } else { vec![Button::Level] };
    for b in pills {
        let label = if b == Button::Level { c.pill } else { b.label() };
        let bw = (btf.width(label) as f64 / s) as i32 + (if b == Button::Level { 44.0 } else { 28.0 } * k) as i32;
        buttons.push((b, Rect { x: fx, y: fy, w: bw, h: pill_h }));
        fx += bw + GAP / 2;
    }
    let esc_w = (sf.width(Button::Close.label()) as f64 / s) as i32 + 8;
    let esc = Rect { x: pad + inner - esc_w, y: fy, w: esc_w, h: pill_h };
    if c.buttons.contains(&Button::Close) {
        buttons.push((Button::Close, esc));
    }
    let cloud = c.cloud.map(|_| Rect { x: fx + GAP / 2, y: fy, w: (esc.x - fx - GAP).max(1), h: pill_h });
    y = fy + pill_h + pad - 6;
    Geo { panel: Rect { x: 0, y: 0, w, h: y }, head, field, hint, tools, tool_lines, title, body, lines, body_rows, note, keys, buttons, cloud }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 2000, h: 1125 };

    fn content<'a>(body: &'a str, buttons: &'a [Button]) -> Content<'a> {
        Content { hint: Some("Enter ohne Text"), tools: &[], title: None, body, note: None, buttons, keys: None, pill: "Fragt vorher", levels: false, cloud: Some("geht an Claude (Cloud)"), big: false, max_h: 1000 }
    }

    #[test]
    fn the_popup_comes_from_the_top_in_the_middle_and_leaves_what_it_frames() {
        let r = place(SCREEN, 36, &[], 760, 300);
        assert_eq!((r.x, r.y), ((2000 - 760) / 2, 36), "under the top bar, in the middle");
        let framed = Rect { x: 900, y: 100, w: 200, h: 40 };
        let r = place(SCREEN, 36, &[framed], 760, 300);
        assert_eq!(overlap(r, framed), 0, "at the bottom when the top would cover the framed element: {r:?}");
        assert!(place(SCREEN, 36, &[], 760, 5000).h <= SCREEN.h - 36, "never taller than the screen");
    }

    #[test]
    fn the_parts_are_stacked_inside_the_panel() {
        let t = Theme::default();
        let tools = ["Befehl bits dateien suchen downloads --art pdf".to_string()];
        let buttons = [Button::StepYes, Button::StepNo, Button::Close];
        let c = Content { tools: &tools, title: Some("Schritt 1"), keys: Some("Enter = Ja · Esc = Nein"), note: Some("Hinweis"), ..content("Ich schreibe „Georg“ in die Adresszeile und schicke ab.", &buttons) };
        let g = geo(&t, &c, |w, h| Rect { x: 100, y: 50, w, h });
        let inside = |r: Rect| r.x >= g.panel.x && r.y >= g.panel.y && r.x + r.w <= g.panel.x + g.panel.w && r.y + r.h <= g.panel.y + g.panel.h;
        for r in [g.head, g.field, g.hint.unwrap(), g.tools.unwrap(), g.title.unwrap(), g.body, g.note.unwrap(), g.keys.unwrap(), g.cloud.unwrap()] {
            assert!(inside(r), "{r:?} in {:?}", g.panel);
        }
        for (b, r) in &g.buttons {
            assert!(inside(*r), "{b:?} {r:?}");
        }
        assert!(g.field.y + g.field.h <= g.hint.unwrap().y && g.tools.unwrap().y + g.tools.unwrap().h <= g.body.y, "board order");
        let pill = g.buttons.iter().find(|(b, _)| *b == Button::Level).unwrap().1;
        let esc = g.buttons.iter().find(|(b, _)| *b == Button::Close).unwrap().1;
        assert!(pill.x < esc.x && pill.y == esc.y, "the pill left, Esc right, in one footer");
        assert!(Button::StepYes.consents() && !Button::StepNo.consents() && Button::of_word("automodus") == Some(Button::LevelPick(Level::Auto)));
    }

    #[test]
    fn a_long_answer_scrolls_and_the_field_buttons_and_footer_stay_inside() {
        let t = Theme::default();
        let body = (1..=60).map(|i| format!("Satz {i} der langen Antwort.")).collect::<Vec<_>>().join("\n");
        for big in [false, true] {
            let c = Content { big, max_h: max_height(SCREEN, 36), ..content(&body, &[Button::Keyboard, Button::Close]) };
            let g = geo(&t, &c, |w, h| place(SCREEN, 36, &[], w, h));
            assert!(g.body_rows < g.lines.len() && g.body_rows >= 2, "the body scrolls: {} of {}", g.body_rows, g.lines.len());
            let inside = |r: Rect| r.y >= g.panel.y && r.y + r.h <= g.panel.y + g.panel.h;
            assert!(inside(g.body) && inside(g.field), "{:?} {:?} in {:?}", g.body, g.field, g.panel);
            for (b, r) in &g.buttons {
                assert!(inside(*r), "{b:?} {r:?} in {:?}", g.panel);
            }
            assert!(g.panel.y + g.panel.h <= SCREEN.h, "on the screen: {:?}", g.panel);
        }
    }

    #[test]
    fn the_three_levels_replace_the_pill_when_it_is_open() {
        let t = Theme::default();
        let c = Content { levels: true, ..content("", &[Button::Close]) };
        let g = geo(&t, &c, |w, h| Rect { x: 0, y: 0, w, h });
        let picks: Vec<Button> = g.buttons.iter().map(|(b, _)| *b).filter(|b| matches!(b, Button::LevelPick(_))).collect();
        assert_eq!(picks, Level::ALL.map(Button::LevelPick));
        assert!(!g.buttons.iter().any(|(b, _)| *b == Button::Level));
    }
}
