//! A (Georg 12.11.: closing a workspace): its windows move into another workspace. Nothing is closed: the tiles
//! (with their tab stacks) come after the target's tiles, the floats over the target's floats with their
//! rectangles, the minimised windows join the target's list and go back after the moved tiles. A window that was
//! fullscreen there is an ordinary one here (its tile or float is kept). The target keeps its focus; an empty
//! target focuses what came in, as the moved workspace had it.

use super::{Focus, Workspace, mins::Back};

impl<W: Clone + PartialEq> Workspace<W> {
    pub fn absorb(&mut self, from: Workspace<W>) {
        let (tiles, floats) = (self.tiles.len(), self.floating.len());
        let was_empty = tiles == 0 && floats == 0;
        self.tiles.extend(from.tiles);
        self.floating.extend(from.floating);
        self.minimized.extend(from.minimized.into_iter().map(|mut m| {
            if let Back::Tile { at, .. } = &mut m.back {
                *at += tiles;
            }
            m
        }));
        if was_empty {
            self.focus = match from.focus {
                Focus::Tile(i) => Focus::Tile(tiles + i),
                Focus::Float(i) => Focus::Float(floats + i),
                Focus::None => Focus::None,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Floating, Rect, Tile};
    use super::*;

    fn ws(tiles: &[&[u32]], floats: &[u32]) -> Workspace<u32> {
        let mut w = Workspace::default();
        for s in tiles {
            w.tiles.push(Tile { stack: s.to_vec(), top: s.len() - 1, weight: 1.0 });
        }
        for f in floats {
            w.floating.push(Floating { win: *f, rect: Rect { x: *f as i32, y: 10, w: 300, h: 200 }, restore: None });
        }
        w
    }

    #[test]
    fn tiles_tabs_floats_and_minimised_windows_move_and_nothing_is_lost() {
        let mut to = ws(&[&[1], &[2, 3]], &[4]);
        to.focus = Focus::Tile(1);
        let mut from = ws(&[&[10, 11], &[12]], &[13]);
        from.minimize(&12);
        from.minimize(&13);
        from.focus = Focus::Tile(0);
        let all = |w: &Workspace<u32>| {
            let mut v: Vec<u32> = w.all_windows().copied().collect();
            v.sort();
            v
        };
        let mut before: Vec<u32> = all(&to).into_iter().chain(all(&from)).collect();
        before.sort();
        to.absorb(from);
        assert_eq!(all(&to), before, "every window is there");
        assert_eq!(to.tiles.iter().map(|t| t.stack.clone()).collect::<Vec<_>>(), vec![vec![1], vec![2, 3], vec![10, 11]], "the tab stack stays one tile");
        assert_eq!(to.floating.iter().map(|f| f.win).collect::<Vec<_>>(), vec![4]);
        assert_eq!(to.minimized.iter().map(|m| m.win).collect::<Vec<_>>(), vec![12, 13]);
        assert_eq!(to.focus, Focus::Tile(1), "the target keeps its focus");
        // The minimised tile goes back after the moved tiles, the float with its rectangle.
        assert!(to.restore(&12));
        assert_eq!(to.tiles.last().unwrap().stack, vec![12]);
        assert!(to.restore(&13));
        assert_eq!(to.floating.last().map(|f| (f.win, f.rect.x)), Some((13, 13)));
    }

    #[test]
    fn an_empty_target_takes_the_focus_of_what_came_in() {
        let mut to: Workspace<u32> = Workspace::default();
        let mut from = ws(&[&[1], &[2]], &[3]);
        from.focus = Focus::Float(0);
        to.absorb(from);
        assert_eq!(to.focus, Focus::Float(0));
        let mut to = ws(&[], &[]);
        let mut from = ws(&[&[1], &[2]], &[]);
        from.focus = Focus::Tile(1);
        to.absorb(from);
        assert_eq!(to.focus, Focus::Tile(1));
    }
}
