//! Rectangles of the screen and what is done with them: where Aqua's own windows are, the part of a picture that is made black before it is read,
//! and how large a picture may be.

use serde::Serialize;

/// A rectangle in pixels of the screen. `x` and `y` can be negative (a second screen to the left of the first or above it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i64 {
        self.x as i64 + self.w as i64
    }

    pub fn bottom(&self) -> i64 {
        self.y as i64 + self.h as i64
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// The part that both rectangles cover, or `None` when they only touch or do not meet.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let left = (self.x as i64).max(other.x as i64);
        let top = (self.y as i64).max(other.y as i64);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > left && bottom > top).then(|| Rect { x: left as i32, y: top as i32, w: (right - left) as u32, h: (bottom - top) as u32 })
    }

    /// The smallest rectangle that holds both.
    pub fn union(&self, other: &Rect) -> Rect {
        let left = (self.x as i64).min(other.x as i64);
        let top = (self.y as i64).min(other.y as i64);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect { x: left as i32, y: top as i32, w: (right - left) as u32, h: (bottom - top) as u32 }
    }

    /// The middle, which is where a click on it would go.
    pub fn center(&self) -> (i32, i32) {
        (self.x + (self.w / 2) as i32, self.y + (self.h / 2) as i32)
    }
}

/// The longest side the Windows text reader takes (it says so itself; this is what it said on the machines this was tried on).
pub const MAX_SIDE: u32 = 10_000;
/// The most pixels one picture may have: a 5K screen is 14.7 million, and a picture of 24 million is 96 MB.
pub const MAX_PIXELS: u64 = 24_000_000;

/// A picture of part of the screen: the pixels as Windows gives them (blue, green, red, alpha, row after row, no padding at the end of a row) and where on the screen
/// they were. The alpha of every pixel is opaque: a picture whose alpha is 0 reads as nothing.
#[derive(Debug, Clone)]
pub struct Shot {
    pub area: Rect,
    pub bgra: Vec<u8>,
    /// How many of Aqua's own windows were over the picture and were made black (see [`Shot::hide`]).
    pub hidden: usize,
}

impl Shot {
    /// A picture from its pixels; `Err` when they are not as many as the area says.
    pub fn new(area: Rect, bgra: Vec<u8>) -> Result<Shot, String> {
        let want = area.area() as usize * 4;
        if area.w == 0 || area.h == 0 || bgra.len() != want {
            return Err(format!("a picture of {}x{} pixels has {} bytes, not {}", area.w, area.h, want, bgra.len()));
        }
        Ok(Shot { area, bgra, hidden: 0 })
    }

    /// Make black the parts of the picture that the rectangles (in pixels of the screen) cover, and count how many of the rectangles were over it. Aqua's own windows
    /// are hidden this way before the picture is read: the person's screen is read, not Aqua's own words and questions, which could be read back to the model as if somebody
    /// else had written them.
    pub fn hide(&mut self, rects: &[Rect]) -> usize {
        let mut touched = 0;
        for r in rects {
            let Some(part) = r.intersect(&self.area) else { continue };
            touched += 1;
            for row in 0..part.h as usize {
                let y = (part.y as i64 - self.area.y as i64) as usize + row;
                let x = (part.x as i64 - self.area.x as i64) as usize;
                let start = (y * self.area.w as usize + x) * 4;
                for px in self.bgra[start..start + part.w as usize * 4].chunks_exact_mut(4) {
                    px.copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
        self.hidden += touched;
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_meet_and_join_as_they_should_even_when_they_are_left_of_or_above_the_origin() {
        let a = Rect::new(-100, -50, 200, 100);
        let b = Rect::new(50, 0, 100, 100);
        assert_eq!(a.intersect(&b), Some(Rect::new(50, 0, 50, 50)));
        assert_eq!(a.union(&b), Rect::new(-100, -50, 250, 150));
        // They only touch: nothing is shared. They do not meet: nothing is shared.
        assert_eq!(Rect::new(0, 0, 10, 10).intersect(&Rect::new(10, 0, 10, 10)), None);
        assert_eq!(Rect::new(0, 0, 10, 10).intersect(&Rect::new(50, 50, 10, 10)), None);
        assert_eq!(Rect::new(10, 20, 31, 41).center(), (25, 40));
        // The largest sizes do not overflow.
        let big = Rect::new(i32::MAX - 5, i32::MIN, u32::MAX, u32::MAX);
        assert!(big.right() > i32::MAX as i64 && big.area() > 0);
        let _ = big.intersect(&Rect::new(0, 0, 10, 10));
    }

    #[test]
    fn a_picture_has_the_bytes_its_size_says_and_hiding_blanks_only_what_is_covered() {
        assert!(Shot::new(Rect::new(0, 0, 4, 3), vec![0; 47]).is_err());
        assert!(Shot::new(Rect::new(0, 0, 0, 3), vec![]).is_err());
        let mut shot = Shot::new(Rect::new(100, 200, 4, 3), vec![200; 4 * 3 * 4]).unwrap();
        // One rectangle over the middle two pixels of the second row, one that is not over the picture at all, one that covers a corner and goes beyond it.
        let touched = shot.hide(&[Rect::new(101, 201, 2, 1), Rect::new(0, 0, 50, 50), Rect::new(103, 202, 10, 10)]);
        assert_eq!((touched, shot.hidden), (2, 2));
        let px = |x: usize, y: usize| shot.bgra[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(px(1, 1), vec![0, 0, 0, 255]);
        assert_eq!(px(2, 1), vec![0, 0, 0, 255]);
        assert_eq!(px(3, 2), vec![0, 0, 0, 255]);
        assert_eq!(px(0, 1), vec![200; 4], "next to it is as it was");
        assert_eq!(px(3, 1), vec![200; 4]);
        assert_eq!(px(0, 0), vec![200; 4]);
        assert_eq!(shot.bgra.iter().filter(|b| **b == 0).count(), 3 * 3, "three pixels are black, with three zero bytes each");
    }
}
