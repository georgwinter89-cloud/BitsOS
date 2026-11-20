//! DB1: what is on top at a point, stacked as the drawing stacks it (paint::order_with): the sofa layer, the
//! desktop (its overlays and dialogs, and its bars: those lie under a floating window, the one place this order is
//! not exact), the floating windows (the last raised first), then the tiled ones. `ui-act` clicks an element only
//! where it is on top; Georg's own press is credited to the element on top.

use uilist::{Element, Window as UiWindow};

use crate::layout::{Place, Placement};
use crate::state::App;
use crate::ui_reg::{DESKTOP, Full, SOFA, wid};
use crate::win::Window;

impl App {
    /// The window ids, top first.
    pub(crate) fn ui_stack(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.sofa.show() != crate::sofa::Show::None {
            out.push(SOFA.to_string());
        }
        out.push(DESKTOP.to_string());
        let drawn: Vec<Placement<Window>> = self.ui_drawn().into_iter().filter(|p| p.visible).collect();
        let floating = |p: &&Placement<Window>| matches!(p.place, Place::Floating { .. });
        out.extend(drawn.iter().rev().filter(floating).map(|p| wid(&p.win)));
        out.extend(drawn.iter().rev().filter(|p| !floating(p)).map(|p| wid(&p.win)));
        out
    }

    /// The window on top at (x, y): the first with any shown element there (a panel covers what is under it);
    /// never asks a provider.
    pub(crate) fn ui_top_at(&self, x: f64, y: f64) -> Option<UiWindow> {
        self.ui_stack().into_iter().find_map(|id| self.ui_windows(&Full::Local(id.clone())).into_iter().find(|w| w.window == id && w.at(x, y).is_some()))
    }

    /// The element a click at (x, y) reaches: in the window on top, the innermost one there that acts (a group
    /// around it, such as a bar, does not take the click).
    pub(crate) fn ui_click_at(&self, x: f64, y: f64) -> Option<(UiWindow, Element)> {
        let w = self.ui_top_at(x, y)?;
        let e = w.at_where(x, y, &|e| !e.actions.is_empty())?.clone();
        Some((w, e))
    }
}

/// Whether `id` is `e` or lies inside it.
pub(crate) fn holds(e: &Element, id: &str) -> bool {
    e.id == id || e.children.iter().any(|c| holds(c, id))
}
