//! CL-3b: drawing the conversation of `assist_layout::Look` into a sidebar's image: the classic bar's
//! (`chrome_draw::side`) or the Bühne's panel (`panel`, the right column once an assistant answers).
//! Nothing here moves: the busy ring is its own sprite, so a growing answer redraws only when a line came.

use crate::assist::ToolState;
use crate::assist_layout::{CardKind, LINE, Look, Part, SMALL, Style, button_font, small_font, text_font};
use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::deco::hud;
use crate::layout::Rect;
use crate::text::{Family, Weight};
use crate::theme::{Style as Label, Theme, alpha};

fn mark_colour(s: ToolState, t: &Theme) -> crate::theme::Rgba {
    match s {
        ToolState::Asks => t.warm,
        ToolState::Runs => t.claude,
        ToolState::Done => t.accent,
        ToolState::Refused | ToolState::Stopped => t.muted,
    }
}

pub fn conversation(c: &mut Canvas, p: &Px, t: &Theme, l: &Look) {
    let (tf, sf, bf) = (text_font(t), small_font(t), button_font(t));
    let top = |lh: i32, f: &crate::text::Font| (p.v(lh as f32) as i32 - f.height()) / 2;
    for b in &l.bubbles {
        c.rrect(p.b(*b), p.v(10.0), alpha(t.accent_soft, 0.75));
    }
    for (r, s) in &l.marks {
        c.rrect(p.b(*r), p.v(1.5), mark_colour(*s, t));
    }
    for row in &l.rows {
        let (f, lh, col) = match row.style {
            Style::Georg | Style::Assistant => (&tf, LINE, t.text),
            Style::Tool(ToolState::Asks) => (&sf, SMALL, t.warm),
            Style::Tool(_) | Style::Note { error: false } => (&sf, SMALL, t.muted),
            Style::Note { error: true } => (&sf, SMALL, t.danger),
        };
        let at = p.r(Rect { x: row.x, y: row.y, w: 1, h: lh });
        c.text(f, at.x, at.y + top(lh, f), &row.text, col);
    }
    if let Some(h) = &l.hint {
        c.text_fit(&tf, p.r(Rect { h: LINE, ..l.list }), h, t.muted);
    }
    if let Some(card) = &l.card {
        let b = p.b(card.rect);
        let question = matches!(card.kind, CardKind::Question { .. });
        hud::shadow(c, b, p.v(12.0), &t.shadow_raised, t);
        c.rrect(b, p.v(12.0), alpha(t.accent_soft, 0.75));
        // A question waits for Georg: the warm edge of the ring's waiting state.
        c.rrect_line(b, p.v(12.0), p.line(), if question { alpha(t.warm, 0.9) } else { alpha(t.accent, 0.35) });
        let (x, mut y) = (card.rect.x + 14, card.rect.y + 12);
        if let Some(title) = &card.title {
            let at = p.r(Rect { x, y, w: 1, h: SMALL });
            c.text(&sf, at.x, at.y + top(SMALL, &sf), title, t.warm);
            y += SMALL + 2;
        }
        for line in &card.lines {
            let at = p.r(Rect { x, y, w: 1, h: LINE });
            c.text(&tf, at.x, at.y + top(LINE, &tf), line, t.text);
            y += LINE;
        }
        if !card.mark.is_empty() {
            y += 4;
        }
        for line in &card.mark {
            let at = p.r(Rect { x, y, w: 1, h: SMALL });
            c.text(&sf, at.x, at.y + top(SMALL, &sf), line, t.warm);
            y += SMALL;
        }
        match &card.kind {
            CardKind::Question { yes, no, yes_picked, armed, note } => {
                // Ja is pale until it counts (a second after the card came or moved).
                crate::install_draw::button(c, p, t, *yes, "Ja", *yes_picked, *armed && l.hover == Some(Part::Yes), *armed);
                crate::install_draw::button(c, p, t, *no, "Nein", !*yes_picked, l.hover == Some(Part::No), true);
                let mut ny = y + 6;
                for line in note {
                    let at = p.r(Rect { x, y: ny, w: 1, h: SMALL });
                    c.text(&sf, at.x, at.y + top(SMALL, &sf), line, t.danger);
                    ny += SMALL;
                }
            }
            CardKind::Info { link: Some(r) } => {
                let at = p.r(*r);
                let w = c.text_fit(&tf, at, crate::assist_layout::LINK_TEXT, t.accent);
                let under = at.y + (at.h + tf.height()) / 2 + 1;
                c.fill(at.x, under, w, p.line().round() as i32, if l.hover == Some(Part::Link) { t.accent } else { alpha(t.accent, 0.5) });
            }
            CardKind::Info { link: None } => {}
        }
    }
    if let Some((r, lines, error)) = &l.note {
        for (i, line) in lines.iter().enumerate() {
            let at = p.r(Rect { y: r.y + i as i32 * SMALL, h: SMALL, ..*r });
            c.text(&sf, at.x, at.y + top(SMALL, &sf), line, if *error { t.danger } else { t.muted });
        }
    }
    let i = &l.input;
    let b = p.b(i.rect);
    c.rrect(b, p.v(8.0), t.panel2);
    c.rrect_line(b, p.v(8.0), p.line(), if i.focus || l.hover == Some(Part::Input) { t.accent } else { t.line });
    let inner = |k: usize| p.r(Rect { x: i.rect.x + 10, y: i.rect.y + 8 + k as i32 * LINE, w: i.rect.w - 20, h: LINE });
    if !i.placeholder.is_empty() {
        c.text_fit(&tf, inner(0), &i.placeholder, t.muted);
    }
    for (k, line) in i.lines.iter().enumerate() {
        let at = inner(k);
        c.text(&tf, at.x, at.y + top(LINE, &tf), line.trim_end_matches('\n'), if l.live { t.text } else { t.muted });
    }
    if let Some((k, x)) = i.caret {
        let at = inner(k);
        let ch = tf.height();
        c.fill(at.x + x, at.y + (at.h - ch) / 2, p.line().round() as i32 + 1, ch, t.accent);
    }
    crate::page_draw::button(c, p, t, l.new, crate::assist_layout::NEW_TEXT, l.hover == Some(Part::New), true, &bf);
    crate::page_draw::button(c, p, t, l.send, l.send_label, l.hover == Some(Part::Send), l.live, &bf);
    side(c, p, t, l);
}

/// EINS-B (board 3): the chats' list, the switch "Fenster vorne mitsehen", the footer's pill and cloud line, the
/// trash's notice.
fn side(c: &mut Canvas, p: &Px, t: &Theme, l: &Look) {
    let (tf, sf, bf) = (text_font(t), small_font(t), button_font(t));
    if let Some(h) = l.chats_head {
        c.text_fit(&sf, p.r(h), "Chats", t.muted);
    }
    for (i, ch) in l.chats.iter().enumerate() {
        let b = p.b(ch.rect);
        if ch.current || l.hover == Some(Part::Chat(i)) {
            c.rrect(b, p.v(8.0), if ch.current { t.chip } else { alpha(t.chip, 0.5) });
        }
        let inner = Rect { x: ch.rect.x + 8, w: ch.rect.w - 40, ..ch.rect };
        c.text_fit(&tf, p.r(Rect { h: LINE, y: inner.y + 2, ..inner }), &ch.title, t.text);
        c.text_fit(&sf, p.r(Rect { h: SMALL, y: inner.y + 2 + LINE, ..inner }), &ch.sub, t.muted);
        c.text_mid(&tf, p.r(ch.remove), "×", if l.hover == Some(Part::ChatRemove(i)) { t.danger } else { t.muted });
    }
    if let Some(s) = &l.sees {
        let r = p.b(s.switch);
        c.rrect(r, r.h / 2.0, if s.on { t.accent } else { t.panel2 });
        c.rrect_line(r, r.h / 2.0, p.line(), t.line_strong);
        let knob = if s.on { r.x + r.w - r.h / 2.0 } else { r.x + r.h / 2.0 };
        c.disc(knob, r.y + r.h / 2.0, r.h / 2.0 - p.v(3.0), if s.on { t.on_accent } else { t.muted });
        let tx = s.switch.x + s.switch.w + 10;
        let w = (l.body.x + l.body.w - tx - crate::assist_layout::PAD).max(1);
        c.text_fit(&tf, p.r(Rect { x: tx, y: s.switch.y, w, h: LINE }), crate::assist_layout::SEES_TEXT, t.text);
        c.text_fit(&sf, p.r(Rect { x: tx, y: s.switch.y + LINE, w, h: SMALL }), &format!("gerade: {}", s.now), t.muted);
    }
    if l.levels.is_empty() {
        crate::page_draw::button(c, p, t, l.pill, &l.pill_text, l.hover == Some(Part::Level), true, &bf);
    }
    for (lv, r) in &l.levels {
        let on = l.pill_text.starts_with(lv.name());
        let b = p.b(*r);
        c.rrect(b, b.h / 2.0, if on { t.accent_soft } else { t.panel2 });
        c.rrect_line(b, b.h / 2.0, p.line(), if on || l.hover == Some(Part::LevelPick(*lv)) { t.accent } else { t.line });
        c.text_mid(&bf, p.r(*r), lv.name(), t.text);
    }
    if let Some((r, text)) = &l.cloud {
        let at = p.r(*r);
        let dot = p.v(4.5);
        c.disc(at.x as f32 + dot, at.y as f32 + at.h as f32 / 2.0, dot, [0.89, 0.741, 0.373, 1.0]);
        c.text_fit(&sf, Rect { x: at.x + p.v(14.0) as i32, w: (at.w - p.v(14.0) as i32).max(1), ..at }, text, t.muted);
    }
    if let Some((r, text)) = &l.trash {
        let at = p.r(*r);
        let w = c.text_fit(&sf, at, text, t.muted);
        let link = Rect { x: at.x + w + p.v(8.0) as i32, w: (at.w - w - p.v(8.0) as i32).max(1), ..at };
        c.text_fit(&sf, link, crate::assist_layout::TRASH_LINK, if l.hover == Some(Part::Trash) { t.accent } else { alpha(t.accent, 0.8) });
    }
}

/// The Bühne's panel: where it is, whose it is, what the ring says, the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelLook {
    pub rect: Rect,
    pub theme: u64,
    pub name: String,
    pub word: &'static str,
    pub waiting: bool,
    pub notice: Option<String>,
    pub tool: Option<String>,
    pub conv: Look,
    /// The model list right of the name.
    pub menu: Option<crate::assist_menu_app::MenuLook>,
}

/// The panel's head (title, state, the line under them) in logical pixels.
pub fn panel_head(t: &Theme) -> i32 {
    t.bl(22.0 + 24.0 + 8.0) + 2 * SMALL + 6
}

/// The panel as the column's frame: the assistant's name as its title, the state as its label.
pub fn panel(l: &PanelLook, t: &Theme, area: Rect) -> Canvas {
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let kk = (t.board) as f32;
    crate::chrome_draw::buehne::frame(&mut c, p.b(l.rect), t, true);
    let pr = p.r(l.rect);
    let band = Rect { x: pr.x + (24.0 * kk) as i32, y: pr.y + (22.0 * kk) as i32, w: pr.w - (48.0 * kk) as i32, h: (24.0 * kk) as i32 };
    let tw = c.text_fit(&t.bfont(Family::Display, 20.0, Weight::SemiBold), band, &l.name, t.text);
    let rest = Rect { x: band.x + tw + (12.0 * kk) as i32, w: band.w - tw - (12.0 * kk) as i32, ..band };
    c.text_right(&t.style(Label::Label), rest, &Theme::label(Label::Label, l.word), if l.waiting { t.warm } else { t.accent });
    let line = Rect { x: l.rect.x + t.bl(24.0), y: l.rect.y + t.bl(22.0 + 24.0 + 8.0), w: l.rect.w - 2 * t.bl(24.0), h: SMALL };
    head_lines(&mut c, &p, t, line, Rect { y: line.y + SMALL, ..line }, l.notice.as_deref(), l.tool.as_deref());
    conversation(&mut c, &p, t, &l.conv);
    if let Some(m) = &l.menu {
        crate::assist_menu_draw::draw(&mut c, &p, t, m);
    }
    c
}

/// The two lines of a sidebar's head: "geht an <Anbieter>" in the waiting colour, and the running tool's words.
pub fn head_lines(c: &mut Canvas, p: &Px, t: &Theme, notice_at: Rect, tool_at: Rect, notice: Option<&str>, tool: Option<&str>) {
    let sf = small_font(t);
    if let Some(s) = notice {
        c.text_fit(&sf, p.r(notice_at), s, t.warm);
    }
    if let Some(s) = tool {
        c.text_fit(&sf, p.r(tool_at), s, t.text);
    }
}
