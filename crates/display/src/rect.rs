//! Rectangles in pixels. Arithmetic is done in u64 so no input can overflow it.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
    pub fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }
    fn x1(self) -> u64 {
        self.x as u64 + self.w as u64
    }
    fn y1(self) -> u64 {
        self.y as u64 + self.h as u64
    }

    /// The bounding box of both (an empty rectangle counts as nothing).
    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.x1().max(o.x1());
        let y1 = self.y1().max(o.y1());
        Rect { x: x0, y: y0, w: clamp32(x1 - x0 as u64), h: clamp32(y1 - y0 as u64) }
    }

    /// Both share at least one pixel.
    pub fn intersects(self, o: Rect) -> bool {
        !self.is_empty() && !o.is_empty() && (self.x as u64) < o.x1() && (o.x as u64) < self.x1() && (self.y as u64) < o.y1() && (o.y as u64) < self.y1()
    }

    /// They overlap or share an edge (no gap between them on either axis).
    pub fn touches(self, o: Rect) -> bool {
        !self.is_empty() && !o.is_empty() && (self.x as u64) <= o.x1() && (o.x as u64) <= self.x1() && (self.y as u64) <= o.y1() && (o.y as u64) <= self.y1()
    }

    pub fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x && (x as u64) < self.x1() && y >= self.y && (y as u64) < self.y1()
    }

    /// The part inside a `width` x `height` frame.
    pub fn clip(self, width: u32, height: u32) -> Rect {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Rect { x, y, w: self.w.min(width - x), h: self.h.min(height - y) }
    }
}

fn clamp32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_intersect_touch() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(10, 0, 5, 5);
        assert!(!a.intersects(b) && a.touches(b));
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 10));
        assert!(!a.touches(Rect::new(11, 0, 1, 1)));
        assert_eq!(Rect::default().union(b), b);
        assert!(!Rect::default().touches(a));
        let huge = Rect::new(u32::MAX - 1, u32::MAX - 1, u32::MAX, u32::MAX);
        assert_eq!(huge.union(a).w, u32::MAX, "no overflow");
        assert_eq!(huge.clip(100, 50), Rect::new(100, 50, 0, 0));
        assert_eq!(Rect::new(90, 40, 20, 20).clip(100, 50), Rect::new(90, 40, 10, 10));
        assert!(a.contains(9, 9) && !a.contains(10, 0));
    }
}
