//! WY-16: the release path. A buffer goes back to its client (wl_buffer.release, and its explicit-sync release
//! point) only when the renderer is done with it: a commit that replaces or removes a surface's buffer moves the
//! old one here instead of letting Smithay release it at once; everything goes back right after the turn's draw
//! (Pixman composites on the CPU; GLES waits for the frame's sync point and reads the picture back, render_gl.rs)
//! and before its frame callbacks, so a client that reuses a buffer on `done` already has its release (wy/60
//! starved when the release came after the callback); a turn without a frame gives back at its end. The same for shm and dma-buf buffers, and for
//! hung clients' windows (their surfaces are not drawn, so the old buffer goes back at the end of the turn).

use smithay::backend::renderer::utils::{Buffer, RendererSurfaceStateUserData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward};

#[derive(Default)]
pub struct Hold {
    /// Smithay's buffer handles: wl_buffer.release goes out when the last one drops.
    buffers: Vec<Buffer>,
    /// Release points of explicit-sync surfaces, signalled when given back (dropped).
    points: Vec<crate::syncobj::Release>,
}

impl Hold {
    /// Before the commit is applied (`on_commit_buffer_handler`): every surface of the tree whose buffer changes
    /// in this commit keeps its current buffer here.
    pub fn keep_tree(&mut self, surface: &WlSurface) {
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let changes = states.cached_state.get::<SurfaceAttributes>().current().buffer.is_some();
                if !changes {
                    return;
                }
                if let Some(old) = crate::syncobj::on_apply(states) {
                    self.points.push(old);
                }
                if let Some(state) = states.data_map.get::<RendererSurfaceStateUserData>() {
                    // A poisoned lock means a panic elsewhere already; then Smithay releases it as before.
                    if let Ok(s) = state.lock()
                        && let Some(b) = s.buffer()
                    {
                        self.buffers.push(b.clone());
                    }
                }
            },
            |_, _, _| true,
        );
    }

    /// After the turn's frame: the held buffers are released and the release points signalled.
    /// Returns how many of each went back (for `ctl stats`).
    pub fn give_back(&mut self) -> (usize, usize) {
        let n = (self.buffers.len(), self.points.len());
        self.buffers.clear();
        // In order per timeline (`Release::order_key`): two surfaces or buffers may share one release timeline.
        signal_order(&mut self.points, crate::syncobj::Release::order_key);
        for p in self.points.drain(..) {
            drop(p);
        }
        n
    }

    /// Buffers and points held now (`ctl stats` held_now).
    pub fn len(&self) -> usize {
        self.buffers.len() + self.points.len()
    }
}

/// Sorts release points by (timeline, value), the order they are signalled in.
fn signal_order<T>(v: &mut [T], key: impl Fn(&T) -> (usize, u64)) {
    v.sort_by_key(key);
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_points_signal_upwards_per_timeline() {
        // (timeline, value) as two surfaces sharing timeline 7 hand them in, the higher one first.
        let mut v = vec![(7usize, 9u64), (3, 2), (7, 4), (3, 1)];
        super::signal_order(&mut v, |p| *p);
        assert_eq!(v, [(3, 1), (3, 2), (7, 4), (7, 9)]);
    }

    #[test]
    fn an_empty_hold_gives_back_nothing() {
        let mut h = super::Hold::default();
        assert_eq!(h.give_back(), (0, 0));
        assert_eq!(h.len(), 0);
    }
}
