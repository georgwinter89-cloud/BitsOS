//! The model list right of the assistant's name in the sidebar (Georg, 12.11.: "ein Dropdown Menü wo ich das
//! Modell aussuchen kann"): which models are offered for the provider that answers, and where the closed
//! control and the open list lie. Pure: entries from the provider's kind, its model setting and what it
//! announces; places in logical pixels from the head's band. Never a text field: a model that is not offered
//! is reached through the page "KI".

use crate::layout::Rect;

pub const STANDARD: &str = "Standard";
pub const MORE: &str = "Andere Modelle in den KI-Einstellungen …";
pub const H: i32 = 22;
pub const ROW_H: i32 = 30;
/// Room between the name and the control, and kept free before the state word.
const GAP: i32 = 10;
/// Narrower than this, the control is not shown at all (only a chevron and an ellipsis would be left).
const MIN_W: i32 = 48;
/// The control's inner parts: padding, the gap before the chevron, the chevron.
pub const PAD: i32 = 10;
pub const CHEVRON: i32 = 9;
const PAD_ROW: i32 = 14;
/// Left of a row's text: the mark of the model set now.
pub const MARK: i32 = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// A value for the provider's model setting ("" is the provider's own default) and its label.
    Model { value: String, label: String },
    /// The page "KI" at this provider's model row.
    More,
}

impl Entry {
    pub fn label(&self) -> &str {
        match self {
            Entry::Model { label, .. } => label,
            Entry::More => MORE,
        }
    }
}

/// What a provider of `kind` offers besides "Standard": Anthropic's fixed list, a helper's announced models.
pub fn offered(kind: &str, helper: &[(String, String)]) -> Vec<(String, String)> {
    match kind {
        "anthropic" => crate::ki_kinds::ANTHROPIC_MODELS.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
        crate::ki::PROGRAM => helper.to_vec(),
        _ => Vec::new(),
    }
}

/// "Standard", the offered models, the model set now when it is none of them (as it stands, so a hand-typed
/// one is seen and can be replaced), and, when the kind offers no list, the way to the page "KI".
pub fn entries(current: &str, offered: &[(String, String)]) -> Vec<Entry> {
    let mut out = vec![Entry::Model { value: String::new(), label: STANDARD.into() }];
    out.extend(offered.iter().map(|(v, l)| Entry::Model { value: v.clone(), label: l.clone() }));
    let cur = current.trim();
    if !cur.is_empty() && !offered.iter().any(|(v, _)| v == cur) {
        out.push(Entry::Model { value: cur.into(), label: cur.into() });
    }
    if offered.is_empty() {
        out.push(Entry::More);
    }
    out
}

/// The entry of the model set now.
pub fn current(entries: &[Entry], current: &str) -> Option<usize> {
    entries.iter().position(|e| matches!(e, Entry::Model { value, .. } if value == current.trim()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub closed: Rect,
    /// The open list's frame and its rows (only those that fit).
    pub frame: Rect,
    pub rows: Vec<Rect>,
}

/// The widths the place needs (logical px): the name, the state word kept free at the right, the closed label,
/// and each row's label.
pub struct Widths {
    pub name: i32,
    pub right: i32,
    pub label: i32,
    pub rows: Vec<i32>,
}

/// The control right of the name in the head's `band`; the list under it inside `all` (the panel), `open`.
/// None: no room for it.
pub fn place(band: Rect, all: Rect, w: &Widths, open: bool) -> Option<Place> {
    let x = band.x + w.name.min(band.w) + GAP;
    let room = band.x + band.w - w.right - if w.right > 0 { GAP } else { 0 } - x;
    if room < MIN_W {
        return None;
    }
    let natural = PAD + w.label + 6 + CHEVRON + PAD;
    let closed = Rect { x, y: band.y + (band.h - H) / 2, w: natural.min(room), h: H };
    let (left, right) = (all.x + 8, all.x + all.w - 8);
    let widest = w.rows.iter().copied().max().unwrap_or(0) + 2 * PAD_ROW + MARK;
    let lw = widest.max(closed.w).min(right - left);
    let lx = closed.x.min(right - lw).max(left);
    let top = closed.y + closed.h + 4;
    let bottom = all.y + all.h - 8;
    let mut rows = Vec::new();
    if open {
        let mut y = top + 4;
        for _ in &w.rows {
            if y + ROW_H + 4 > bottom {
                break;
            }
            rows.push(Rect { x: lx, y, w: lw, h: ROW_H });
            y += ROW_H;
        }
    }
    let frame = Rect { x: lx, y: top, w: lw, h: if rows.is_empty() { 0 } else { rows.len() as i32 * ROW_H + 8 } };
    Some(Place { closed, frame, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(e: &[Entry]) -> Vec<&str> {
        e.iter().map(Entry::label).collect()
    }

    #[test]
    fn entries_by_kind_with_the_model_set_now() {
        let helper = vec![("fable".to_string(), "Fable".to_string()), ("opus".to_string(), "Opus".to_string())];
        let e = entries("opus", &offered("program", &helper));
        assert_eq!(labels(&e), ["Standard", "Fable", "Opus"]);
        assert_eq!(current(&e, "opus"), Some(2));
        // A hand-typed value is shown as it stands and can be replaced.
        let e = entries("Fable 5.2", &offered("anthropic", &[]));
        assert_eq!(labels(&e), ["Standard", "Fable 5.2", "Opus 5.5", "Sonnet 5.5", "Haiku 5.5", "Fable 5.2"]);
        assert_eq!(e[1], Entry::Model { value: "claude-fable-5-2".into(), label: "Fable 5.2".into() });
        assert_eq!(current(&e, "Fable 5.2"), Some(5));
        assert_eq!(current(&e, ""), Some(0));
        // Kinds without a list: Standard, the model now, the way to the page; a helper that announces none too.
        assert_eq!(labels(&entries("gpt-x", &offered("openai", &helper))), ["Standard", "gpt-x", MORE]);
        assert_eq!(labels(&entries("", &offered("program", &[]))), ["Standard", MORE]);
        assert_eq!(labels(&entries(" ", &offered("gemini", &[]))), ["Standard", MORE]);
    }

    #[test]
    fn the_control_keeps_off_the_name_and_the_state_word_and_the_list_stays_in_the_panel() {
        let band = Rect { x: 100, y: 20, w: 400, h: 24 };
        let all = Rect { x: 76, y: 0, w: 448, h: 900 };
        let w = Widths { name: 70, right: 120, label: 60, rows: vec![60, 300, 50] };
        let p = place(band, all, &w, false).expect("room");
        assert_eq!(p.closed, Rect { x: 180, y: 21, w: PAD + 60 + 6 + CHEVRON + PAD, h: H });
        assert!(p.rows.is_empty() && p.frame.h == 0);
        let p = place(band, all, &w, true).expect("room");
        assert_eq!(p.rows.len(), 3);
        assert!(p.rows.iter().all(|r| r.x >= all.x + 8 && r.x + r.w <= all.x + all.w - 8), "{:?}", p.rows);
        assert_eq!(p.frame.y, p.closed.y + H + 4);
        // Narrow: the label is cut, the state word keeps its room; narrower still: not shown.
        let narrow = Rect { w: 300, ..band };
        let p = place(narrow, all, &w, false).expect("room");
        assert_eq!(p.closed.x + p.closed.w, narrow.x + narrow.w - 120 - GAP);
        assert!(place(Rect { w: 220, ..band }, all, &w, false).is_none());
        // Short panel: only the rows that fit.
        let p = place(band, Rect { h: 130, ..all }, &w, true).expect("room");
        assert_eq!(p.rows.len(), 2);
    }
}
