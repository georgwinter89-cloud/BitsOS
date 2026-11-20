//! The Feld's rectangles (logical px) from the Feld board's sizes (Feld.dc.html, board px through `Theme::bl`): the
//! library's panel (1120x800, centred under the top bar), its head of 96 with the ring, the text and the Esc chip,
//! the rows (52 high, grouped under their labels), the foot of 96 with the assistant's line and, at its right, the
//! key hints (the design has them under the panel; there the Bühne's own Feld bar lies at 2560x1440). Pure, so the
//! look, the clicks and `ctl hot` read the same rectangles.

use crate::feld::Kind;
use crate::layout::Rect;
use crate::theme::{Style, Theme};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geo {
    pub dim: Rect,
    pub panel: Rect,
    pub head: Rect,
    /// The ring at the head's left.
    pub icon: Rect,
    pub text: Rect,
    pub esc: Rect,
    pub body: Rect,
    pub foot: Rect,
    /// The key hints at the foot's right.
    pub keys: Rect,
}

pub const KEYS: [&str; 3] = ["↑ ↓ wählen", "Enter bestätigen", "Esc schließen"];

/// The key hints as drawn (the label style's capitals, apart).
pub fn keys_text() -> String {
    KEYS.iter().map(|s| Theme::label(Style::Label, s)).collect::<Vec<_>>().join("      ")
}

pub fn geo(full: Rect, top_h: i32, t: &Theme) -> Geo {
    let b = |v: f32| t.bl(v);
    let lib = crate::library_view::geo(full, top_h, t);
    let panel = lib.panel;
    let head = Rect { h: b(96.0).min(panel.h), ..panel };
    let side = b(34.0);
    let icon = Rect { x: panel.x + b(32.0), y: head.y + (head.h - side) / 2, w: side, h: side };
    let label = t.style(Style::Label);
    let esc_w = (label.width(crate::library_draw::ESC) as f64 / t.scale).ceil() as i32 + b(28.0);
    let eh = b(44.0).min(head.h);
    let esc = Rect { x: panel.x + panel.w - b(20.0) - esc_w, y: head.y + (head.h - eh) / 2, w: esc_w, h: eh };
    let tx = icon.x + icon.w + b(18.0);
    let text = Rect { x: tx, y: head.y, w: (esc.x - b(16.0) - tx).max(1), h: head.h };
    let fh = b(96.0).min((panel.h - head.h).max(0));
    let foot = Rect { x: panel.x, y: panel.y + panel.h - fh, w: panel.w, h: fh };
    let top = head.y + head.h + b(20.0);
    let body = Rect { x: panel.x + b(24.0), y: top, w: (panel.w - b(48.0)).max(1), h: (foot.y - b(20.0) - top).max(1) };
    let kw = ((label.width(&keys_text()) as f64 / t.scale).ceil() as i32).min(foot.w / 2);
    let keys = Rect { x: foot.x + foot.w - b(32.0) - kw, y: foot.y, w: kw, h: foot.h };
    Geo { dim: lib.dim, panel, head, icon, text, esc, body, foot, keys }
}

/// Where each row lies in `g.body` (with its group's label when it starts a group), in order; rows that do not fit
/// are left out (the caller keeps the question's row by leaving programs out first).
pub fn place(g: &Geo, kinds: &[Kind], t: &Theme) -> Vec<(Option<Rect>, Rect)> {
    let b = |v: f32| t.bl(v);
    let (label_h, row_h, gap, group_gap) = (b(14.0), b(52.0), b(6.0), b(16.0));
    let end = g.body.y + g.body.h;
    let mut y = g.body.y;
    let mut out = Vec::new();
    for (i, k) in kinds.iter().enumerate() {
        let starts = i == 0 || kinds[i - 1] != *k;
        if starts && i > 0 {
            y += group_gap - gap;
        }
        let label = starts.then(|| Rect { x: g.body.x + b(12.0), y, w: g.body.w - b(24.0), h: label_h });
        let ry = if starts { y + label_h + gap } else { y };
        if ry + row_h > end {
            break;
        }
        out.push((label, Rect { x: g.body.x, y: ry, w: g.body.w, h: row_h }));
        y = ry + row_h + gap;
    }
    out
}

/// The parts of a row (logical px): the colour mark, the title, the quiet detail, the "Enter" hint.
pub fn row_parts(r: Rect, t: &Theme) -> (Rect, Rect, Rect, Rect) {
    let b = |v: f32| t.bl(v);
    let m = b(10.0);
    let mark = Rect { x: r.x + b(14.0), y: r.y + (r.h - m) / 2, w: m, h: m };
    let hint = Rect { x: r.x + r.w - b(14.0) - b(64.0), y: r.y, w: b(64.0), h: r.h };
    let detail_w = (r.w * 2 / 5).max(1);
    let detail = Rect { x: hint.x - b(14.0) - detail_w, y: r.y, w: detail_w, h: r.h };
    let tx = mark.x + mark.w + b(14.0);
    let title = Rect { x: tx, y: r.y, w: (detail.x - b(14.0) - tx).max(1), h: r.h };
    (mark, title, detail, hint)
}
