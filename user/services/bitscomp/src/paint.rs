//! The stacking order of one frame (WY-5c, desktop.md §1), kept apart from drawing so it can be tested: floating
//! windows lie above every bar; only the circle menu (with the drop preview), the address suggestions and the
//! notice, and a window's own hung veil lie above a float.

use crate::layout::{Place, Placement};

/// One layer, in the order the damage tracker wants: topmost first. `usize` is an index into the placements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Cursor,
    /// The circle menu and the drop preview (`deco::overlays`).
    Overlays,
    /// The notice and the address suggestions (`chrome_render::over`).
    Over,
    /// A window's frame (header, border, the corners that round off the client).
    Frame(usize),
    /// A hung window's veil: over its content, under its own frame.
    Veil(usize),
    Content(usize),
    /// The bars and the start page (`chrome_render::elements`).
    Chrome,
}

/// Topmost first: cursor, menu, suggestions, the floats (last raised first), the bars, the tiles. Hidden
/// placements (other stack pages) are left out.
/// WY-8: a fullscreen window is all there is under the cursor, the menu and the notice: no frame, no bars, nothing
/// under it (the windows it covers are hidden).
pub fn order<W>(placements: &[Placement<W>]) -> Vec<Layer> {
    let mut out = vec![Layer::Cursor, Layer::Overlays, Layer::Over];
    if let Some(i) = placements.iter().position(|p| p.fullscreen && p.visible) {
        out.extend([Layer::Veil(i), Layer::Content(i)]);
        return out;
    }
    let shown = || placements.iter().enumerate().rev().filter(|(_, p)| p.visible);
    let window = |i: usize| [Layer::Frame(i), Layer::Veil(i), Layer::Content(i)];
    let floats = shown().filter(|(_, p)| matches!(p.place, Place::Floating { .. }));
    out.extend(floats.flat_map(|(i, _)| window(i)));
    out.push(Layer::Chrome);
    let tiles = shown().filter(|(_, p)| !matches!(p.place, Place::Floating { .. }));
    out.extend(tiles.flat_map(|(i, _)| window(i)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Rect, Workspace};

    fn at(order: &[Layer], l: Layer) -> usize {
        order.iter().position(|&x| x == l).unwrap_or_else(|| panic!("{l:?} not drawn"))
    }

    #[test]
    fn a_fullscreen_window_is_drawn_alone_without_frame_or_bars() {
        let mut ws: Workspace<u32> = Workspace::default();
        for w in 1..=3 {
            ws.add(w);
        }
        let (area, full) = (Rect { x: 0, y: 40, w: 1000, h: 460 }, Rect { x: 0, y: 0, w: 1000, h: 500 });
        ws.toggle_float(&3, area);
        ws.focus_window(&2);
        assert!(ws.set_fullscreen(&2, true));
        let p = ws.arrange(area, full, 8);
        let i = p.iter().position(|q| q.win == 2).unwrap();
        assert_eq!(order(&p), vec![Layer::Cursor, Layer::Overlays, Layer::Over, Layer::Veil(i), Layer::Content(i)]);
        // The focus going to the float ends the cover: the usual order again.
        ws.focus_window(&3);
        let o = order(&ws.arrange(area, full, 8));
        assert!(o.contains(&Layer::Chrome) && o.len() > 5);
    }

    #[test]
    fn floats_lie_over_the_bars_under_the_menu_suggestions_and_their_veil() {
        let mut ws: Workspace<u32> = Workspace::default();
        for w in 1..=4 {
            ws.add(w);
        }
        let area = Rect { x: 0, y: 0, w: 1000, h: 500 };
        ws.toggle_float(&3, area);
        ws.toggle_float(&4, area);
        ws.focus_window(&3);
        let p = ws.arrange(area, area, 8);
        let o = order(&p);
        let idx = |w: u32| p.iter().position(|q| q.win == w).unwrap();
        let (f3, f4, t1) = (idx(3), idx(4), idx(1));
        assert_eq!(o[..3], [Layer::Cursor, Layer::Overlays, Layer::Over], "menu and suggestions over everything");
        for f in [f3, f4] {
            assert!(at(&o, Layer::Over) < at(&o, Layer::Frame(f)));
            assert!(at(&o, Layer::Frame(f)) < at(&o, Layer::Veil(f)), "the header stays over the veil");
            assert!(at(&o, Layer::Veil(f)) < at(&o, Layer::Content(f)), "the veil lies over its own window");
            assert!(at(&o, Layer::Content(f)) < at(&o, Layer::Chrome), "a float lies over every bar");
        }
        assert!(at(&o, Layer::Content(f3)) < at(&o, Layer::Frame(f4)), "the focused float is on top");
        assert!(at(&o, Layer::Chrome) < at(&o, Layer::Frame(t1)), "the bars over the tiles");
        // Another float's veil lies under a float above it.
        assert!(at(&o, Layer::Content(f3)) < at(&o, Layer::Veil(f4)));
    }

    #[test]
    fn hidden_pages_are_not_drawn() {
        let mut ws: Workspace<u32> = Workspace::default();
        ws.add(1);
        ws.add(2);
        ws.focus_window(&2);
        assert!(ws.stack_focused());
        let p = ws.arrange(Rect { x: 0, y: 0, w: 1000, h: 500 }, Rect { x: 0, y: 0, w: 1000, h: 500 }, 8);
        let hidden = p.iter().position(|q| !q.visible).expect("a hidden page");
        assert!(!order(&p).contains(&Layer::Content(hidden)));
    }
}
