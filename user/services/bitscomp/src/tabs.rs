//! The tab strip of a window header (plan/spec/desktop.md §1: tabs like a browser's): one tab per stack page,
//! sharing the room between the left padding and the header buttons evenly, between `tab_min` and `tab_max`
//! wide; what does not fit at `tab_min` is counted on a "+N" chip. Pure geometry in logical pixels.

use crate::layout::Rect;
use crate::theme::Theme;
use crate::zones::float_button;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Strip {
    /// Stack index of the first tab drawn.
    pub first: usize,
    /// The tabs drawn, for stack pages `first..first + tabs.len()`.
    pub tabs: Vec<Rect>,
    /// The "+N" chip and N, when not every page has a tab.
    pub more: Option<(Rect, usize)>,
}

impl Strip {
    /// The drawn tab of stack page `page`.
    pub fn tab(&self, page: usize) -> Option<Rect> {
        page.checked_sub(self.first).and_then(|i| self.tabs.get(i).copied())
    }
}

/// The room tabs (and the chip) may use: x from, x to.
fn room(f: Rect, t: &Theme) -> (i32, i32) {
    (f.x + t.border + t.tab_pad, float_button(f, t).x - t.tab_pad)
}

/// The strip of a header `f` for `n` pages of which `shown` is shown; the shown tab is always drawn.
pub fn strip(f: Rect, n: usize, shown: usize, t: &Theme) -> Strip {
    let (x0, x1) = room(f, t);
    let avail = (x1 - x0).max(0);
    let n = n.max(1);
    let fits = |k: i32, w: i32| k * w + (k - 1) * t.tab_gap;
    let even = |k: i32, room: i32| ((room - (k - 1) * t.tab_gap) / k).min(t.tab_max);
    let (count, w, more) = if even(n as i32, avail) >= t.tab_min || n == 1 {
        (n, even(n as i32, avail).max(0), 0)
    } else {
        let room = avail - t.more_width - t.tab_gap;
        let k = ((room + t.tab_gap) / (t.tab_min + t.tab_gap)).clamp(1, n as i32 - 1);
        (k as usize, even(k, room).max(0), n - k as usize)
    };
    let first = (shown + 1).saturating_sub(count).min(n - count);
    let (y, h) = (f.y + t.tab_top, t.header_height - t.tab_top);
    let tabs: Vec<Rect> = (0..count as i32).map(|i| Rect { x: x0 + i * (w + t.tab_gap), y, w, h }).collect();
    let more = (more > 0).then(|| {
        let x = x0 + fits(count as i32, w) + t.tab_gap;
        (Rect { x, y, w: t.more_width.min(x1 - x).max(0), h: h - t.tab_lift }, more)
    });
    Strip { first, tabs, more }
}

/// The small close button at a tab's right.
pub fn close_rect(tab: Rect, t: &Theme) -> Rect {
    let s = t.tab_close;
    let pad = (tab.h - s) / 2;
    Rect { x: tab.x + tab.w - t.tab_inner - s, y: tab.y + pad, w: s, h: s }
}

/// Where a tab's text may go: x from, x to.
pub fn text_room(tab: Rect, t: &Theme) -> (i32, i32) {
    let x0 = tab.x + t.tab_inner + t.tab_dot + 8;
    (x0, (close_rect(tab, t).x - 6).max(x0))
}

/// The name a tab shows: the title, else the app id, else "Fenster".
pub fn label(title: &str, app_id: &str) -> String {
    [title, app_id].into_iter().find(|s| !s.trim().is_empty()).unwrap_or("Fenster").to_string()
}

/// Where a tab dragged inside its own strip would be inserted: a slot 0..=tabs.len() between drawn tabs.
pub fn slot_at(s: &Strip, x: f64) -> usize {
    s.tabs.iter().filter(|r| (r.x as f64 + r.w as f64 / 2.0) < x).count()
}

/// The insert marker for `slot`: a thin bar in the gap before tab `slot` (after the last one for the end).
pub fn slot_bar(s: &Strip, slot: usize, t: &Theme) -> Option<Rect> {
    let edge = match (slot.checked_sub(1).and_then(|i| s.tabs.get(i)), s.tabs.get(slot)) {
        (_, Some(r)) => r.x - t.tab_gap / 2,
        (Some(r), None) => r.x + r.w + t.tab_gap / 2,
        (None, None) => return None,
    };
    let r = s.tabs.first()?;
    Some(Rect { x: edge - 1, y: r.y, w: 3, h: r.h })
}

#[cfg(test)]
#[path = "tabs_tests.rs"]
mod tests;
