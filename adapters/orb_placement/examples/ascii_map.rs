//! Look at what the analysis makes of a picture: `cargo run --example ascii_map -- picture.bgra WIDTH HEIGHT [step]`
//! (picture.bgra is raw top-down BGRA, e.g. written with PIL: img.convert("RGBA") then swap R and B).
use aqua_orb_placement::{BusyMap, Rect, BLOCK};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (w, h): (usize, usize) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let step: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(2);
    let px = std::fs::read(&a[1]).unwrap();
    let t = std::time::Instant::now();
    let m = BusyMap::from_bgra(&px, w, h, w * 4, Rect::new(0, 0, w as i32, h as i32), BLOCK).unwrap();
    println!("analysed {w}x{h} in {:?}; background {}; busy cells {}/{}", t.elapsed(), m.stats.background, m.stats.busy_cells, m.stats.cols * m.stats.rows);
    print!("{}", m.ascii(step));
}
