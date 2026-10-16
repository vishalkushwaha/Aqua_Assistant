//! The decision: keep the orb where it is, or move it, and where to. Pure arithmetic on a handful of candidate spots, so
//! it takes microseconds and can be tested with made-up screens.

use crate::busy::BusyMap;
use crate::geometry::Rect;
use crate::settings::Settings;

/// A spot the person chose before. In order of how specific it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeKind {
    Page,
    App,
    Preferred,
    /// The corner the orb starts in, used when the person has never chosen a spot.
    Default,
}

impl HomeKind {
    fn bonus(self) -> f32 {
        match self {
            HomeKind::Page => 35.0,
            HomeKind::App => 30.0,
            HomeKind::Preferred => 25.0,
            HomeKind::Default => 15.0,
        }
    }

    fn text(self) -> &'static str {
        match self {
            HomeKind::Page => "the spot you chose for this page",
            HomeKind::App => "the spot you chose for this program",
            HomeKind::Preferred => "your preferred spot",
            HomeKind::Default => "the default spot",
        }
    }
}

/// The remembered spots, already turned into rectangles on the screen being looked at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Homes {
    pub page: Option<Rect>,
    pub app: Option<Rect>,
    pub preferred: Option<Rect>,
    pub default: Option<Rect>,
}

impl Homes {
    /// In order of how specific they are.
    fn ordered(&self) -> [(Option<Rect>, HomeKind); 4] {
        [(self.page, HomeKind::Page), (self.app, HomeKind::App), (self.preferred, HomeKind::Preferred), (self.default, HomeKind::Default)]
    }
}

pub struct Input<'a> {
    /// The usable part of the screen the active window is on (no taskbar).
    pub work: Rect,
    /// The part of the orb that matters (the sphere), not its transparent margin.
    pub size: (i32, i32),
    /// The gap kept between the orb and the screen edge.
    pub margin: i32,
    /// Where the orb is now.
    pub current: Rect,
    /// What is busy in the active window. None if it could not be looked at.
    pub busy: Option<&'a BusyMap>,
    pub cursor: Option<(i32, i32)>,
    pub homes: Homes,
    pub fullscreen: bool,
    /// Only spots entirely inside this area may be chosen (and only this area was looked at). None: anywhere on the screen.
    pub restrict: Option<Rect>,
    pub settings: &'a Settings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub moves: bool,
    pub to: Rect,
    pub reason: String,
    pub current_busy: f32,
    pub target_busy: f32,
    pub current_score: f32,
    pub target_score: f32,
    pub considered: usize,
    /// The orb is covering more than the person's sensitivity allows (whether or not a better spot was found).
    pub obstructed: bool,
}

const SAME_SPOT_PX: f32 = 24.0;
/// A remembered spot is only returned to when it is clearly clear, so the orb does not hop back to a spot that is half covered.
const HOME_CLEAR_FACTOR: f32 = 0.6;
const BUSY_WEIGHT: f32 = 160.0;
const CURSOR_PENALTY: f32 = 30.0;
const CURSOR_REACH: i32 = 40;

/// The part of a screen's work area the orb may occupy: the work area less the gap kept at its edges (or all of it if it is too small for that).
pub fn usable_area(work: &Rect, size: (i32, i32), margin: i32) -> Rect {
    let u = work.inflate(-margin);
    if u.w >= size.0 && u.h >= size.1 {
        u
    } else {
        *work
    }
}

/// The eight positions around the edge of the screen (or the four corners in full screen).
fn anchors(u: &Rect, size: (i32, i32), corners_only: bool) -> Vec<Rect> {
    let xs = [u.x, u.x + (u.w - size.0) / 2, u.right() - size.0];
    let ys = [u.y, u.y + (u.h - size.1) / 2, u.bottom() - size.1];
    let mut out = Vec::with_capacity(8);
    for (yi, y) in ys.iter().enumerate() {
        for (xi, x) in xs.iter().enumerate() {
            let centre = xi == 1 && yi == 1;
            let corner = xi != 1 && yi != 1;
            if centre || (corners_only && !corner) {
                continue;
            }
            out.push(Rect::new(*x, *y, size.0, size.1));
        }
    }
    out
}

/// Positions around `center` at a third and at three quarters of the orb's size, in the eight directions.
fn ring(center: &Rect, size: (i32, i32), usable: &Rect, out: &mut Vec<Rect>) {
    for frac in [0.35f32, 0.75] {
        let st = (size.0 as f32 * frac) as i32;
        for (dx, dy) in [(0, -st), (-st, 0), (st, 0), (0, st), (-st, -st), (st, -st), (-st, st), (st, st)] {
            push_unique(out, Rect::new(center.x + dx, center.y + dy, size.0, size.1).clamp_into(usable));
        }
    }
}

fn push_unique(list: &mut Vec<Rect>, r: Rect) {
    if !list.iter().any(|c| c.distance(&r) <= 12.0) {
        list.push(r);
    }
}

fn score(inp: &Input, r: &Rect, busy: f32, reference: Option<&Rect>) -> f32 {
    let mut s = 100.0 - BUSY_WEIGHT * busy;
    let w = &inp.work;
    let near = inp.margin + 8;
    let (l, t, rt, b) = (r.x - w.x <= near, r.y - w.y <= near, w.right() - r.right() <= near, w.bottom() - r.bottom() <= near);
    if l || t || rt || b {
        s += 10.0;
    }
    if (l || rt) && (t || b) {
        s += 4.0;
    }
    if inp.settings.avoid_cursor {
        if let Some((cx, cy)) = inp.cursor {
            if r.inflate(CURSOR_REACH).contains(cx, cy) {
                s -= CURSOR_PENALTY;
            }
        }
    }
    for (home, kind) in inp.homes.ordered() {
        if let Some(h) = home {
            if r.distance(&h) <= SAME_SPOT_PX {
                s += kind.bonus();
                break;
            }
        }
    }
    // Among equally good spots, the nearest to where the person had it (their preferred spot, else where it is now) wins.
    if let Some(reference) = reference {
        s -= (r.distance(reference) / 50.0).min(40.0);
    }
    s
}

pub fn decide(inp: &Input) -> Decision {
    let s = inp.settings;
    let usable = usable_area(&inp.work, inp.size, inp.margin);
    let busy_of = |r: &Rect| if s.avoid_content { inp.busy.map_or(0.0, |b| b.busy_fraction(r)) } else { 0.0 };
    let obstructed_above = s.sensitivity.obstructed_above();
    let margin_pts = s.sensitivity.margin();

    let cur = inp.current;
    let cur_busy = busy_of(&cur);
    let off_screen = cur.fraction_inside(&inp.work) < 0.5;
    let reference = inp.homes.preferred.or(inp.homes.default).unwrap_or(cur);
    let cur_score = score(inp, &cur, cur_busy, Some(&reference));

    let mk = |moves: bool, to: Rect, reason: String, considered: usize| -> Decision {
        let target_busy = if moves { busy_of(&to) } else { cur_busy };
        let target_score = if moves { score(inp, &to, target_busy, Some(&reference)) } else { cur_score };
        Decision { moves, to: if moves { to } else { cur }, reason, current_busy: cur_busy, target_busy, current_score: cur_score, target_score, considered, obstructed: cur_busy > obstructed_above }
    };

    // Remembered spots, most specific first. Not used in full screen, where only the corners are offered.
    let mut homes: Vec<(HomeKind, Rect)> = Vec::new();
    if !inp.fullscreen {
        for (h, kind) in inp.homes.ordered() {
            if let Some(h) = h {
                homes.push((kind, h.clamp_into(&usable)));
            }
        }
    }

    // 1. Back to a remembered spot, if it is clear in this window.
    if !off_screen {
        for (kind, h) in &homes {
            if inp.restrict.is_some_and(|r| !r.covers(h)) {
                continue;
            }
            if busy_of(h) <= obstructed_above * HOME_CLEAR_FACTOR {
                return if h.distance(&cur) > SAME_SPOT_PX {
                    mk(true, *h, format!("{} is clear here", kind.text()), 1)
                } else {
                    mk(false, cur, format!("the orb is on {}", kind.text()), 1)
                };
            }
        }
        // 2. Nothing to do if the orb is not in the way.
        if cur_busy <= obstructed_above {
            let why = if inp.busy.is_none() && s.avoid_content { "the window could not be looked at, so the orb stays where it is" } else { "the orb is not covering anything" };
            return mk(false, cur, why.to_string(), 0);
        }
    }

    // 3. Look for a better spot.
    let mut cands = anchors(&usable, inp.size, inp.fullscreen);
    for (_, h) in &homes {
        push_unique(&mut cands, *h);
    }
    if !inp.fullscreen {
        // Close alternatives next to the first remembered spot, and next to where the orb is now ("slightly higher, slightly left").
        if let Some((_, first)) = homes.first() {
            ring(first, inp.size, &usable, &mut cands);
        }
        if !off_screen {
            ring(&cur, inp.size, &usable, &mut cands);
        }
    }
    if !off_screen {
        push_unique(&mut cands, cur);
    }
    if let Some(r) = inp.restrict {
        cands.retain(|c| r.covers(c));
    }
    // Which spot is "nearest" is measured from where the orb is now, so it moves as little as it can.
    let near = if off_screen { reference } else { cur };
    // A spot only qualifies if it is clearly less covered than where the orb is (or the orb is off this screen). Among the
    // qualifying spots the score decides: nearer to the person's own spot, clear of the pointer, near an edge.
    let mut best: Option<(Rect, f32, f32)> = None;
    for c in &cands {
        let b = busy_of(c);
        let qualifies = off_screen || (c.distance(&cur) > SAME_SPOT_PX && BUSY_WEIGHT * (cur_busy - b) >= margin_pts);
        if !qualifies {
            continue;
        }
        let sc = score(inp, c, b, Some(&near));
        if best.map_or(true, |(_, _, bs)| sc > bs) {
            best = Some((*c, b, sc));
        }
    }
    let n = cands.len();
    let Some((to, to_busy, _)) = best else {
        return mk(false, cur, "no spot is clearly better than this one".to_string(), n);
    };
    if off_screen {
        return mk(true, to, "the orb was not on this screen".to_string(), n);
    }
    let pct = (cur_busy * 100.0).round() as i32;
    mk(true, to, format!("the orb covered content ({pct}% of it); this spot is clearer ({}%)", (to_busy * 100.0).round() as i32), n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Sensitivity;

    const WORK: Rect = Rect::new(0, 0, 1920, 1040);
    const SIZE: (i32, i32) = (220, 220);
    const MARGIN: i32 = 16;

    /// A busy map of the whole screen with the given rectangles marked busy.
    fn screen(busy: &[Rect]) -> BusyMap {
        let (cols, rows) = (240usize, 130usize);
        let mut cells = vec![false; cols * rows];
        for r in busy {
            for y in (r.y / 8)..((r.bottom() + 7) / 8).min(rows as i32) {
                for x in (r.x / 8)..((r.right() + 7) / 8).min(cols as i32) {
                    cells[y as usize * cols + x as usize] = true;
                }
            }
        }
        BusyMap::from_cells(cols, rows, &cells, WORK, 8, 20)
    }

    fn bottom_right() -> Rect {
        Rect::new(WORK.right() - MARGIN - SIZE.0, WORK.bottom() - MARGIN - SIZE.1, SIZE.0, SIZE.1)
    }
    fn top_right() -> Rect {
        Rect::new(WORK.right() - MARGIN - SIZE.0, MARGIN, SIZE.0, SIZE.1)
    }
    fn bottom_left() -> Rect {
        Rect::new(MARGIN, WORK.bottom() - MARGIN - SIZE.1, SIZE.0, SIZE.1)
    }

    fn input<'a>(current: Rect, busy: Option<&'a BusyMap>, settings: &'a Settings) -> Input<'a> {
        Input { work: WORK, size: SIZE, margin: MARGIN, current, busy, cursor: None, homes: Homes::default(), fullscreen: false, restrict: None, settings }
    }

    #[test]
    fn an_orb_that_covers_nothing_stays_put() {
        let map = screen(&[Rect::new(400, 100, 1000, 700)]);
        let s = Settings::default();
        let d = decide(&input(bottom_right(), Some(&map), &s));
        assert!(!d.moves && d.to == bottom_right(), "{d:?}");
    }

    #[test]
    fn an_orb_on_content_moves_to_the_nearest_clear_spot() {
        // The page fills the bottom right; the top right is empty. From the bottom right the nearest corner is top right.
        let map = screen(&[Rect::new(1000, 500, 920, 540)]);
        let s = Settings::default();
        let d = decide(&input(bottom_right(), Some(&map), &s));
        assert!(d.moves, "{d:?}");
        assert_eq!(d.to, top_right(), "{d:?}");
        assert!(d.target_busy < 0.02 && d.current_busy > 0.9);
        assert!(d.reason.contains("covered content"), "{}", d.reason);
    }

    /// A busy map of the whole screen where everything is busy except the given rectangles.
    fn all_busy_except(clear: &[Rect]) -> BusyMap {
        let (cols, rows) = (240usize, 130usize);
        let mut cells = vec![true; cols * rows];
        for r in clear {
            for y in (r.y / 8)..((r.bottom() + 7) / 8).min(rows as i32) {
                for x in (r.x / 8)..((r.right() + 7) / 8).min(cols as i32) {
                    cells[y as usize * cols + x as usize] = false;
                }
            }
        }
        BusyMap::from_cells(cols, rows, &cells, WORK, 8, 20)
    }

    #[test]
    fn a_small_improvement_does_not_move_the_orb() {
        // 32% of the orb is on content (over the 22% limit), and the best other spot is 25% busy: not worth moving for.
        let br = bottom_right();
        let tr = top_right();
        let map = all_busy_except(&[Rect::new(br.x, br.y, 220, 150), Rect::new(tr.x, tr.y, 220, 165)]);
        let s = Settings::default();
        let d = decide(&input(br, Some(&map), &s));
        assert!(!d.moves, "{d:?}");
        assert!(d.current_busy > 0.25 && d.current_busy < 0.34, "{}", d.current_busy);
        // A clearly clearer spot is worth it.
        let map = all_busy_except(&[Rect::new(br.x, br.y, 220, 100), Rect::new(tr.x, tr.y, 220, 220)]); // 55% covered, and one spot is clear
        let d = decide(&input(br, Some(&map), &s));
        assert!(d.moves && d.to == tr, "{d:?}");
    }

    #[test]
    fn a_barely_clearer_spot_is_not_worth_moving_to_even_if_it_is_the_default_corner() {
        // The orb covers 35% of content in the middle right; the default bottom right covers 34%: the person's "home" must not pull it there.
        let mr = Rect::new(WORK.right() - MARGIN - SIZE.0, 410, 220, 220);
        let br = bottom_right();
        let map = all_busy_except(&[Rect::new(mr.x, mr.y, 220, 143), Rect::new(br.x, br.y, 220, 145)]);
        let s = Settings::default();
        let mut i = input(mr, Some(&map), &s);
        i.homes.default = Some(br);
        let d = decide(&i);
        assert!(d.current_busy > 0.3 && d.current_busy < 0.4, "{d:?}");
        assert!(!d.moves, "{d:?}");
    }

    #[test]
    fn low_sensitivity_leaves_a_partly_covered_orb_where_high_moves_it() {
        let map = screen(&[Rect::new(bottom_right().x, bottom_right().y, 220, 60)]); // ~29% of the orb
        let mut s = Settings::default();
        s.sensitivity = Sensitivity::Low;
        assert!(!decide(&input(bottom_right(), Some(&map), &s)).moves, "low keeps it");
        s.sensitivity = Sensitivity::High;
        assert!(decide(&input(bottom_right(), Some(&map), &s)).moves, "high moves it");
    }

    #[test]
    fn it_goes_back_to_the_preferred_spot_when_that_is_clear() {
        // The orb was moved to the top right earlier; the person prefers the bottom left, which is clear in this window.
        let map = screen(&[Rect::new(600, 300, 1000, 600)]);
        let s = Settings::default();
        let mut i = input(top_right(), Some(&map), &s);
        i.homes.preferred = Some(bottom_left());
        let d = decide(&i);
        assert!(d.moves && d.to == bottom_left(), "{d:?}");
        assert!(d.reason.contains("preferred"), "{}", d.reason);
        // Already there: stays.
        let mut i = input(bottom_left(), Some(&map), &s);
        i.homes.preferred = Some(bottom_left());
        assert!(!decide(&i).moves);
    }

    #[test]
    fn a_preferred_spot_that_is_covered_is_not_returned_to() {
        let map = screen(&[Rect::new(0, 700, 700, 340)]); // covers the bottom left
        let s = Settings::default();
        let mut i = input(top_right(), Some(&map), &s);
        i.homes.preferred = Some(bottom_left());
        let d = decide(&i);
        assert!(!d.moves, "the orb is fine where it is: {d:?}");
    }

    #[test]
    fn a_page_spot_beats_a_program_spot_beats_the_preferred_spot_when_clear() {
        let map = screen(&[]);
        let s = Settings::default();
        let page = Rect::new(MARGIN, MARGIN, 220, 220);
        let mut i = input(bottom_right(), Some(&map), &s);
        i.homes = Homes { page: Some(page), app: Some(bottom_left()), preferred: Some(top_right()), default: None };
        assert_eq!(decide(&i).to, page);
        // The page spot is covered: the program spot is next.
        let map = screen(&[Rect::new(0, 0, 400, 400)]);
        let mut i = input(bottom_right(), Some(&map), &s);
        i.homes = Homes { page: Some(page), app: Some(bottom_left()), preferred: Some(top_right()), default: None };
        assert_eq!(decide(&i).to, bottom_left());
        // Both covered: the preferred spot.
        let map = screen(&[Rect::new(0, 0, 400, 400), Rect::new(0, 700, 400, 340)]);
        let mut i = input(bottom_right(), Some(&map), &s);
        i.homes = Homes { page: Some(page), app: Some(bottom_left()), preferred: Some(top_right()), default: None };
        assert_eq!(decide(&i).to, top_right());
    }

    #[test]
    fn with_no_preferred_spot_the_orb_returns_to_the_default_corner_when_it_is_clear() {
        let map = screen(&[Rect::new(600, 300, 1000, 600)]); // content in the middle only
        let s = Settings::default();
        let mut i = input(top_right(), Some(&map), &s);
        i.homes.default = Some(bottom_right());
        let d = decide(&i);
        assert!(d.moves && d.to == bottom_right() && d.reason.contains("default"), "{d:?}");
        // A preferred spot wins over the default.
        i.homes.preferred = Some(bottom_left());
        assert_eq!(decide(&i).to, bottom_left());
        // The default corner is covered: stay where the orb is.
        let map = screen(&[Rect::new(1000, 600, 920, 440)]);
        let mut i = input(top_right(), Some(&map), &s);
        i.homes.default = Some(bottom_right());
        assert!(!decide(&i).moves);
    }

    #[test]
    fn an_orb_that_is_not_on_this_screen_is_brought_in() {
        let map = screen(&[]);
        let s = Settings::default();
        let d = decide(&input(Rect::new(3000, 200, 220, 220), Some(&map), &s));
        assert!(d.moves && WORK.contains(d.to.x, d.to.y) && d.to.right() <= WORK.right(), "{d:?}");
        assert!(d.reason.contains("not on this screen"));
        // Half on the screen is still "on" it (the person may have parked it against the edge).
        assert!(!decide(&input(Rect::new(1810, 200, 220, 220), Some(&map), &s)).moves);
    }

    #[test]
    fn full_screen_only_offers_corners() {
        let map = screen(&[Rect::new(700, 300, 600, 400), Rect::new(0, 0, 300, 300)]);
        let s = Settings::default();
        let mut i = input(Rect::new(850, 400, 220, 220), Some(&map), &s);
        i.fullscreen = true;
        let d = decide(&i);
        assert!(d.moves, "{d:?}");
        let corners = [top_right(), bottom_left(), bottom_right()];
        assert!(corners.contains(&d.to), "{:?}", d.to);
    }

    #[test]
    fn with_content_avoidance_off_the_orb_is_left_alone() {
        let map = screen(&[Rect::new(0, 0, 1920, 1040)]);
        let mut s = Settings::default();
        s.avoid_content = false;
        assert!(!decide(&input(bottom_right(), Some(&map), &s)).moves);
    }

    #[test]
    fn if_the_window_could_not_be_looked_at_the_orb_stays() {
        let s = Settings::default();
        let d = decide(&input(bottom_right(), None, &s));
        assert!(!d.moves);
        assert!(d.reason.contains("could not be looked at"), "{}", d.reason);
    }

    #[test]
    fn among_clear_spots_the_one_nearest_the_default_corner_wins_even_when_all_are_far() {
        // Text fills the right of the screen: the three left-hand corners are clear. From the bottom right the bottom left is nearest.
        let map = screen(&[Rect::new(860, 0, 1060, 1040)]);
        let s = Settings::default();
        let mut i = input(bottom_right(), Some(&map), &s);
        i.homes.default = Some(bottom_right());
        let d = decide(&i);
        assert!(d.moves && d.to == bottom_left(), "{d:?}");
    }

    #[test]
    fn a_nearby_spot_is_preferred_to_a_far_corner() {
        // Content has scrolled up under the lower part of the orb. Slightly higher is clear: go there, not to the top right corner.
        let br = bottom_right();
        let map = screen(&[Rect::new(1600, 880, 320, 160)]);
        let s = Settings::default();
        let d = decide(&input(br, Some(&map), &s));
        assert!(d.moves, "{d:?}");
        assert_eq!(d.to, Rect::new(br.x, br.y - 165, 220, 220), "{d:?}");
        assert!(d.obstructed && d.to.distance(&br) < 200.0);
    }

    #[test]
    fn nearby_spots_are_tried_before_far_ones_whichever_way_is_clear() {
        // Content below and to the left, clear above and to the right: it goes up, not across the screen.
        let br = bottom_right();
        let map = screen(&[Rect::new(0, 840, 1920, 200), Rect::new(1300, 400, 300, 640)]);
        let s = Settings::default();
        let d = decide(&input(br, Some(&map), &s));
        assert!(d.moves && d.to.x == br.x && d.to.y < br.y, "{d:?}");
    }

    #[test]
    fn a_restricted_look_never_picks_a_spot_outside_the_area_that_was_looked_at() {
        // Only the area around the orb was analysed, so far corners look "empty" for lack of information. They must not be chosen.
        let br = bottom_right();
        let map = screen(&[Rect::new(1000, 500, 920, 540)]); // everything near the orb is covered
        let s = Settings::default();
        let mut i = input(br, Some(&map), &s);
        i.restrict = Some(br.inflate(165).intersect(&WORK).unwrap());
        i.homes.default = Some(top_right());
        let d = decide(&i);
        assert!(!d.moves && d.obstructed, "nothing nearby is clear, so the caller must look wider: {d:?}");
        i.restrict = None;
        assert!(decide(&i).moves, "with the whole screen in view there is a clear spot");
    }

    #[test]
    fn a_restricted_look_still_moves_to_a_clear_spot_inside_its_area() {
        let br = bottom_right();
        let map = screen(&[Rect::new(1600, 880, 320, 160)]);
        let s = Settings::default();
        let mut i = input(br, Some(&map), &s);
        i.restrict = Some(br.inflate(165).intersect(&WORK).unwrap());
        let d = decide(&i);
        assert!(d.moves && i.restrict.unwrap().covers(&d.to), "{d:?}");
    }

    #[test]
    fn the_cursor_breaks_a_tie_between_two_clear_spots() {
        let map = screen(&[Rect::new(1000, 500, 920, 540)]); // bottom right covered; top right and bottom left are both clear
        let mut s = Settings::default();
        let mut i = input(bottom_right(), Some(&map), &s);
        assert_eq!(decide(&i).to, top_right(), "nearest without the cursor");
        i.cursor = Some((top_right().x + 100, top_right().y + 100));
        let d = decide(&i);
        assert!(d.to != top_right() && d.target_busy < 0.02, "the cursor is on the top right: {d:?}");
        s.avoid_cursor = false;
        let mut i = input(bottom_right(), Some(&map), &s);
        i.cursor = Some((top_right().x + 100, top_right().y + 100));
        assert_eq!(decide(&i).to, top_right(), "cursor avoidance off");
    }

    #[test]
    fn the_cursor_alone_never_moves_the_orb() {
        let map = screen(&[]);
        let s = Settings::default();
        let mut i = input(bottom_right(), Some(&map), &s);
        i.cursor = Some((bottom_right().x + 50, bottom_right().y + 50));
        assert!(!decide(&i).moves);
    }

    #[test]
    fn a_tiny_work_area_does_not_panic() {
        let map = screen(&[]);
        let s = Settings::default();
        let mut i = input(Rect::new(0, 0, 220, 220), Some(&map), &s);
        i.work = Rect::new(0, 0, 150, 150);
        let d = decide(&i);
        assert!(d.to.w == 220);
    }
}
