//! Which parts of a window are "busy" (text, images, controls) and which are empty background, worked out from one
//! picture of that window. No accessibility tree, no AI: the picture is cut into small blocks and a block counts as
//! busy when it has fine detail (text, edges, photos) or when it differs a lot from the window's usual background
//! (a picture, a card, a coloured bar). Everything after that is arithmetic on a summed-area table.

use crate::geometry::Rect;

/// Width and height of one block of the picture, in screen pixels.
pub const BLOCK: usize = 8;
/// Average difference between neighbouring pixels in a block above which the block has fine detail.
const TEXTURE_MIN: f32 = 2.0;
/// How far (0 to 255) a block's brightness may differ from the background before it counts as content.
const DEVIATION_MIN: i32 = 28;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapStats {
    pub cols: usize,
    pub rows: usize,
    pub busy_cells: usize,
    /// The brightness (0 to 255) taken as the window's background.
    pub background: u8,
    /// Almost nothing in the picture was busy: a blank or all-black picture tells us nothing, and the caller can say so.
    pub blank: bool,
    /// Only the area around a region was analysed; everything outside it counts as empty.
    pub local: bool,
}

#[derive(Debug, Clone)]
pub struct BusyMap {
    origin: Rect,
    cell: i32,
    cols: usize,
    rows: usize,
    /// Summed-area table of busy cells, (cols + 1) x (rows + 1).
    sat: Vec<u32>,
    pub stats: MapStats,
}

fn luma(row: &[u8], x: usize) -> u32 {
    // BGRA. 0.114 B + 0.587 G + 0.299 R in 8-bit fixed point.
    (29 * u32::from(row[4 * x]) + 150 * u32::from(row[4 * x + 1]) + 77 * u32::from(row[4 * x + 2])) >> 8
}

impl BusyMap {
    /// Analyse a top-down BGRA picture of the window. `origin` is where the picture's top-left corner is on the screen.
    /// Returns None if the picture is too small to say anything or the buffer does not hold what it claims.
    pub fn from_bgra(pix: &[u8], width: usize, height: usize, stride: usize, origin: Rect, block: usize) -> Option<BusyMap> {
        Self::from_bgra_region(pix, width, height, stride, origin, block, None, None)
    }

    /// Like `from_bgra`, but only the blocks that overlap `region` (screen coordinates) are analysed, which is much cheaper
    /// when only the area around the orb matters. The rest counts as empty, so a caller must not judge spots outside the
    /// region. `background` is the window's background brightness from an earlier full analysis; without it the most
    /// common brightness inside the region is used.
    #[allow(clippy::too_many_arguments)]
    pub fn from_bgra_region(pix: &[u8], width: usize, height: usize, stride: usize, origin: Rect, block: usize, region: Option<&Rect>, background: Option<u8>) -> Option<BusyMap> {
        if block == 0 || width < block * 4 || height < block * 4 || stride < width * 4 || pix.len() < stride * (height - 1) + width * 4 {
            return None;
        }
        let cols = width.div_ceil(block);
        let rows = height.div_ceil(block);
        let (bx0, bx1, by0, by1) = match region {
            Some(r) => {
                let loc = Rect::new(r.x - origin.x, r.y - origin.y, r.w, r.h).intersect(&Rect::new(0, 0, width as i32, height as i32))?;
                (loc.x as usize / block, (loc.right() as usize).div_ceil(block).min(cols), loc.y as usize / block, (loc.bottom() as usize).div_ceil(block).min(rows))
            }
            None => (0, cols, 0, rows),
        };
        let analysed = |i: usize| {
            let (bx, by) = (i % cols, i / cols);
            bx >= bx0 && bx < bx1 && by >= by0 && by < by1
        };
        let mut sum = vec![0u32; cols * rows];
        let mut grad = vec![0u32; cols * rows];
        let mut cnt = vec![0u32; cols * rows];
        for by in by0..by1 {
            let y0 = by * block;
            let y1 = (y0 + block).min(height);
            for y in y0..y1 {
                let row = &pix[y * stride..y * stride + width * 4];
                let below = (y + 1 < y1).then(|| &pix[(y + 1) * stride..(y + 1) * stride + width * 4]);
                for bx in bx0..bx1 {
                    let x0 = bx * block;
                    let x1 = (x0 + block).min(width);
                    let mut s = 0u32;
                    let mut g = 0u32;
                    let mut prev = luma(row, x0);
                    s += prev;
                    for x in x0 + 1..x1 {
                        let l = luma(row, x);
                        s += l;
                        g += l.abs_diff(prev);
                        prev = l;
                    }
                    if let Some(b) = below {
                        for x in x0..x1 {
                            g += luma(b, x).abs_diff(luma(row, x));
                        }
                    }
                    let i = by * cols + bx;
                    sum[i] += s;
                    grad[i] += g;
                    cnt[i] += (x1 - x0) as u32;
                }
            }
        }
        let mean: Vec<u8> = (0..cols * rows).map(|i| (sum[i] / cnt[i].max(1)) as u8).collect();
        let texture: Vec<f32> = (0..cols * rows).map(|i| grad[i] as f32 / (2.0 * cnt[i].max(1) as f32)).collect();

        // The background is the most common brightness among the blocks with no fine detail.
        let mut hist = [0u32; 32];
        let mut flat = 0u32;
        for i in 0..cols * rows {
            if analysed(i) && texture[i] < TEXTURE_MIN {
                hist[(mean[i] >> 3) as usize] += 1;
                flat += 1;
            }
        }
        if flat == 0 {
            for (i, m) in mean.iter().enumerate() {
                if analysed(i) {
                    hist[(m >> 3) as usize] += 1;
                }
            }
        }
        let mut bin = 0usize;
        for (i, n) in hist.iter().enumerate() {
            if *n > hist[bin] {
                bin = i;
            }
        }
        let background = background.unwrap_or((bin * 8 + 4) as u8);

        let busy: Vec<bool> = (0..cols * rows).map(|i| analysed(i) && (texture[i] >= TEXTURE_MIN || (i32::from(mean[i]) - i32::from(background)).abs() >= DEVIATION_MIN)).collect();
        let mut map = Self::from_cells(cols, rows, &busy, origin, block as i32, background);
        if region.is_some() {
            map.stats.local = true;
            map.stats.blank = false;
        }
        Some(map)
    }

    /// A map from cells that are already classified (row by row).
    pub fn from_cells(cols: usize, rows: usize, busy: &[bool], origin: Rect, cell: i32, background: u8) -> BusyMap {
        let mut sat = vec![0u32; (cols + 1) * (rows + 1)];
        for r in 0..rows {
            let mut run = 0u32;
            for c in 0..cols {
                run += u32::from(busy[r * cols + c]);
                sat[(r + 1) * (cols + 1) + c + 1] = sat[r * (cols + 1) + c + 1] + run;
            }
        }
        let busy_cells = sat[(rows + 1) * (cols + 1) - 1] as usize;
        let blank = busy_cells * 100 < cols * rows;
        BusyMap { origin, cell: cell.max(1), cols, rows, sat, stats: MapStats { cols, rows, busy_cells, background, blank, local: false } }
    }

    fn sum(&self, c0: usize, r0: usize, c1: usize, r1: usize) -> u32 {
        let w = self.cols + 1;
        self.sat[r1 * w + c1] + self.sat[r0 * w + c0] - self.sat[r0 * w + c1] - self.sat[r1 * w + c0]
    }

    /// The share (0.0 to 1.0) of `r` that is covered by busy blocks. The part of `r` outside the picture counts as empty.
    pub fn busy_fraction(&self, r: &Rect) -> f32 {
        let Some(clip) = r.intersect(&Rect::new(self.origin.x, self.origin.y, self.cols as i32 * self.cell, self.rows as i32 * self.cell)) else {
            return 0.0;
        };
        let c0 = ((clip.x - self.origin.x) / self.cell) as usize;
        let r0 = ((clip.y - self.origin.y) / self.cell) as usize;
        let c1 = (((clip.right() - self.origin.x) + self.cell - 1) / self.cell) as usize;
        let r1 = (((clip.bottom() - self.origin.y) + self.cell - 1) / self.cell) as usize;
        let (c1, r1) = (c1.min(self.cols), r1.min(self.rows));
        let cells = (c1 - c0) * (r1 - r0);
        if cells == 0 {
            return 0.0;
        }
        let inside = self.sum(c0, r0, c1, r1) as f32 / cells as f32;
        inside * (clip.area() as f32 / r.area().max(1) as f32)
    }

    /// One character per cell, for looking at what the analysis decided (`#` busy, `.` empty).
    pub fn ascii(&self, step: usize) -> String {
        let mut out = String::new();
        let step = step.max(1);
        for r in (0..self.rows).step_by(step) {
            for c in (0..self.cols).step_by(step) {
                out.push(if self.sum(c, r, c + 1, r + 1) > 0 { '#' } else { '.' });
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Img {
        w: usize,
        h: usize,
        px: Vec<u8>,
    }

    impl Img {
        fn new(w: usize, h: usize, v: u8) -> Img {
            let mut px = vec![255u8; w * h * 4];
            for i in 0..w * h {
                px[4 * i] = v;
                px[4 * i + 1] = v;
                px[4 * i + 2] = v;
            }
            Img { w, h, px }
        }
        fn set(&mut self, x: usize, y: usize, v: u8) {
            let i = 4 * (y * self.w + x);
            self.px[i] = v;
            self.px[i + 1] = v;
            self.px[i + 2] = v;
        }
        fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, v: u8) {
            for yy in y..y + h {
                for xx in x..x + w {
                    self.set(xx, yy, v);
                }
            }
        }
        /// Lines of "text": 2 px tall strokes every 5 px, with gaps, like a paragraph.
        fn text(&mut self, x: usize, y: usize, w: usize, h: usize, v: u8) {
            let mut yy = y;
            while yy + 2 <= y + h {
                let mut xx = x;
                while xx + 3 <= x + w {
                    self.rect(xx, yy, 3, 2, v);
                    xx += 5;
                }
                yy += 5;
            }
        }
        fn map(&self) -> BusyMap {
            BusyMap::from_bgra(&self.px, self.w, self.h, self.w * 4, Rect::new(0, 0, self.w as i32, self.h as i32), BLOCK).unwrap()
        }
    }

    #[test]
    fn text_on_a_light_page_is_busy_and_the_margin_is_not() {
        let mut img = Img::new(800, 600, 245);
        img.text(300, 100, 400, 300, 30);
        let m = img.map();
        assert!(m.busy_fraction(&Rect::new(320, 120, 200, 200)) > 0.9);
        assert!(m.busy_fraction(&Rect::new(20, 20, 200, 200)) < 0.02);
        assert!(m.busy_fraction(&Rect::new(20, 380, 220, 200)) < 0.02);
    }

    #[test]
    fn a_flat_picture_on_a_dark_page_counts_as_content() {
        // Dark mode page with a bright poster in the middle (no fine detail inside it, but not background either).
        let mut img = Img::new(800, 600, 14);
        img.rect(300, 40, 300, 520, 235);
        let m = img.map();
        assert!(m.stats.background < 40, "background should be the dark page, not the poster: {}", m.stats.background);
        assert!(m.busy_fraction(&Rect::new(340, 100, 200, 200)) > 0.9);
        assert!(m.busy_fraction(&Rect::new(30, 300, 200, 200)) < 0.02);
    }

    #[test]
    fn a_little_noise_in_the_background_is_not_content() {
        let mut img = Img::new(400, 400, 20);
        for y in 0..400 {
            for x in 0..400 {
                if (x * 7 + y * 13) % 5 == 0 {
                    img.set(x, y, 21);
                }
            }
        }
        let m = img.map();
        assert_eq!(m.stats.busy_cells, 0);
        assert!(m.stats.blank);
    }

    #[test]
    fn the_picture_can_sit_anywhere_on_the_screen() {
        let mut img = Img::new(400, 400, 240);
        img.text(0, 0, 400, 200, 20);
        let m = BusyMap::from_bgra(&img.px, 400, 400, 1600, Rect::new(1000, 500, 400, 400), BLOCK).unwrap();
        assert!(m.busy_fraction(&Rect::new(1050, 520, 100, 100)) > 0.9);
        assert!(m.busy_fraction(&Rect::new(1050, 750, 100, 100)) < 0.05);
        assert_eq!(m.busy_fraction(&Rect::new(0, 0, 100, 100)), 0.0, "outside the picture is empty");
        // Half in, half out: the part outside counts as empty.
        let half = m.busy_fraction(&Rect::new(1350, 500, 100, 100));
        assert!(half > 0.4 && half < 0.55, "{half}");
    }

    #[test]
    fn analysing_only_a_region_gives_the_same_answer_inside_it_and_nothing_outside() {
        let mut img = Img::new(800, 600, 245);
        img.text(0, 0, 800, 600, 30); // text everywhere
        let region = Rect::new(400, 300, 200, 200);
        let full = img.map();
        let local = BusyMap::from_bgra_region(&img.px, 800, 600, 3200, Rect::new(0, 0, 800, 600), BLOCK, Some(&region), None).unwrap();
        assert!(local.stats.local && !full.stats.local);
        assert!(local.busy_fraction(&Rect::new(420, 320, 100, 100)) > 0.9);
        assert_eq!(local.busy_fraction(&Rect::new(20, 20, 100, 100)), 0.0, "outside the region is not analysed");
        assert!(full.busy_fraction(&Rect::new(20, 20, 100, 100)) > 0.9);
        // A region completely outside the picture cannot be analysed.
        assert!(BusyMap::from_bgra_region(&img.px, 800, 600, 3200, Rect::new(0, 0, 800, 600), BLOCK, Some(&Rect::new(2000, 2000, 50, 50)), None).is_none());
    }

    #[test]
    fn a_known_background_makes_a_region_that_is_all_picture_still_count_as_content() {
        // A flat bright picture fills the region; the window's background is dark. With only the region to go on, the picture
        // would itself look like the background, so the caller passes the background it found earlier.
        let mut img = Img::new(800, 600, 14);
        img.rect(300, 40, 300, 520, 235);
        let region = Rect::new(330, 100, 200, 200);
        let guess = BusyMap::from_bgra_region(&img.px, 800, 600, 3200, Rect::new(0, 0, 800, 600), BLOCK, Some(&region), None).unwrap();
        let known = BusyMap::from_bgra_region(&img.px, 800, 600, 3200, Rect::new(0, 0, 800, 600), BLOCK, Some(&region), Some(12)).unwrap();
        assert!(guess.busy_fraction(&Rect::new(340, 110, 100, 100)) < 0.1, "without the hint the picture looks like background");
        assert!(known.busy_fraction(&Rect::new(340, 110, 100, 100)) > 0.9);
    }

    #[test]
    fn a_picture_too_small_or_too_short_is_refused() {
        assert!(BusyMap::from_bgra(&[0; 100], 10, 10, 40, Rect::new(0, 0, 10, 10), BLOCK).is_none());
        let img = Img::new(100, 100, 0);
        assert!(BusyMap::from_bgra(&img.px[..1000], 100, 100, 400, Rect::new(0, 0, 100, 100), BLOCK).is_none());
    }
}
