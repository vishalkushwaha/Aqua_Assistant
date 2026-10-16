//! Rectangles in physical screen pixels (the same units Windows hands out to a per-monitor DPI aware program).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn area(&self) -> i64 {
        i64::from(self.w.max(0)) * i64::from(self.h.max(0))
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Whether `o` lies entirely inside this rectangle.
    pub fn covers(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }

    pub fn inflate(&self, n: i32) -> Rect {
        Rect::new(self.x - n, self.y - n, self.w + 2 * n, self.h + 2 * n)
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    /// Distance between the two rectangles' top-left corners.
    pub fn distance(&self, o: &Rect) -> f32 {
        let dx = (self.x - o.x) as f32;
        let dy = (self.y - o.y) as f32;
        (dx * dx + dy * dy).sqrt()
    }

    /// The same size, moved the least that puts it inside `outer` (a rectangle bigger than `outer` is lined up with its top-left).
    pub fn clamp_into(&self, outer: &Rect) -> Rect {
        let x = if self.w >= outer.w { outer.x } else { self.x.clamp(outer.x, outer.right() - self.w) };
        let y = if self.h >= outer.h { outer.y } else { self.y.clamp(outer.y, outer.bottom() - self.h) };
        Rect::new(x, y, self.w, self.h)
    }

    /// How much of this rectangle lies inside `outer`, 0.0 to 1.0.
    pub fn fraction_inside(&self, outer: &Rect) -> f32 {
        if self.area() == 0 {
            return 0.0;
        }
        self.intersect(outer).map(|i| i.area() as f32 / self.area() as f32).unwrap_or(0.0)
    }
}

/// One display. `work` is the part not taken by the taskbar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: String,
    pub rect: Rect,
    pub work: Rect,
    pub primary: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_and_clamping() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 60, 100, 100);
        assert_eq!(a.intersect(&b), Some(Rect::new(50, 60, 50, 40)));
        assert_eq!(a.intersect(&Rect::new(100, 0, 10, 10)), None, "touching edges do not overlap");
        let outer = Rect::new(0, 0, 1920, 1040);
        assert_eq!(Rect::new(1900, 1000, 220, 220).clamp_into(&outer), Rect::new(1700, 820, 220, 220));
        assert_eq!(Rect::new(-50, -50, 220, 220).clamp_into(&outer), Rect::new(0, 0, 220, 220));
        assert_eq!(Rect::new(5, 5, 3000, 3000).clamp_into(&outer), Rect::new(0, 0, 3000, 3000));
    }

    #[test]
    fn fraction_inside_counts_only_the_visible_part() {
        let screen = Rect::new(0, 0, 1000, 1000);
        assert_eq!(Rect::new(900, 0, 200, 100).fraction_inside(&screen), 0.5);
        assert_eq!(Rect::new(2000, 0, 10, 10).fraction_inside(&screen), 0.0);
        assert_eq!(Rect::new(0, 0, 0, 0).fraction_inside(&screen), 0.0);
    }
}
