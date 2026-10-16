//! A designed text graphic, drawn for real: a gradient background, a headline that wraps to fit, an optional
//! subtitle, in a colour scheme. Deterministic: the same request draws the same bytes.

use super::{check_size, ImageError, ImageProvider, ImageRequest, ImageResult};
use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use image::{Rgb, RgbImage};
use std::path::{Path, PathBuf};

pub struct CardProvider {
    font_bytes: Option<Vec<u8>>,
    searched: Vec<PathBuf>,
}

/// Fonts to try, in order: `AQUA_FONT`, then what ships with the system.
fn font_candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("AQUA_FONT") {
        v.push(PathBuf::from(p));
    }
    for p in [
        "C:/Windows/Fonts/segoeuib.ttf", "C:/Windows/Fonts/arialbd.ttf", "C:/Windows/Fonts/calibrib.ttf", "C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf", "/Library/Fonts/Arial Bold.ttf", "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    ] {
        v.push(PathBuf::from(p));
    }
    v
}

impl CardProvider {
    pub fn system() -> Self {
        Self::with_candidates(font_candidates())
    }

    pub fn with_candidates(candidates: Vec<PathBuf>) -> Self {
        let font_bytes = candidates.iter().find_map(|p| std::fs::read(p).ok().filter(|b| FontVec::try_from_vec(b.clone()).is_ok()));
        Self { font_bytes, searched: candidates }
    }
}

/// (top-left, bottom-right, text, accent)
fn palette(style: &str) -> ([u8; 3], [u8; 3], [u8; 3], [u8; 3]) {
    match style.trim().to_lowercase().as_str() {
        "sunset" => ([255, 138, 76], [178, 44, 110], [255, 255, 255], [255, 226, 170]),
        "forest" => ([20, 83, 45], [6, 40, 40], [240, 253, 244], [134, 239, 172]),
        "slate" => ([51, 65, 85], [15, 23, 42], [248, 250, 252], [148, 163, 184]),
        "violet" => ([109, 40, 217], [49, 46, 129], [250, 245, 255], [216, 180, 254]),
        _ => ([14, 116, 144], [8, 47, 73], [240, 249, 255], [125, 211, 252]), // ocean
    }
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> Rgb<u8> {
    let f = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round().clamp(0.0, 255.0) as u8;
    Rgb([f(0), f(1), f(2)])
}

fn text_width(font: &impl Font, scale: PxScale, s: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut w = 0.0;
    let mut last = None;
    for c in s.chars() {
        let id = scaled.glyph_id(c);
        if let Some(l) = last {
            w += scaled.kern(l, id);
        }
        w += scaled.h_advance(id);
        last = Some(id);
    }
    w
}

/// Break `text` into lines no wider than `max`. A single word wider than that is split by letters.
fn wrap(font: &impl Font, scale: PxScale, text: &str, max: f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while text_width(font, scale, &word) > max && word.chars().count() > 1 {
            // Fit as many letters as possible.
            let mut cut = 1;
            while cut < word.chars().count() && text_width(font, scale, &word.chars().take(cut + 1).collect::<String>()) <= max {
                cut += 1;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            lines.push(word.chars().take(cut).collect());
            word = word.chars().skip(cut).collect();
        }
        let candidate = if cur.is_empty() { word.clone() } else { format!("{cur} {word}") };
        if text_width(font, scale, &candidate) <= max || cur.is_empty() {
            cur = candidate;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn draw_line(img: &mut RgbImage, font: &impl Font, scale: PxScale, text: &str, x: f32, baseline: f32, color: [u8; 3]) {
    let scaled = font.as_scaled(scale);
    let mut caret = x;
    let mut last = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(l) = last {
            caret += scaled.kern(l, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, baseline));
        caret += scaled.h_advance(id);
        last = Some(id);
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                    let p = img.get_pixel_mut(px as u32, py as u32);
                    for i in 0..3 {
                        p.0[i] = (p.0[i] as f32 * (1.0 - cov) + color[i] as f32 * cov).round() as u8;
                    }
                }
            });
        }
    }
}

impl ImageProvider for CardProvider {
    fn name(&self) -> &'static str {
        "card"
    }

    fn ready(&self) -> Result<(), ImageError> {
        if self.font_bytes.is_some() {
            Ok(())
        } else {
            Err(ImageError::NotSetUp(format!("no image generator is set up: drawing a card needs a font, and none was found (looked for: {}). Set AQUA_FONT to a .ttf file.", self.searched.iter().take(3).map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))))
        }
    }

    fn generate(&self, req: &ImageRequest, dir: &Path, stem: &str) -> Result<ImageResult, ImageError> {
        check_size(req.width, req.height)?;
        self.ready()?;
        let title = if req.title.trim().is_empty() { req.prompt.trim() } else { req.title.trim() };
        if title.is_empty() {
            return Err(ImageError::Invalid("invalid setting: a card needs a title (or a prompt to use as one)".into()));
        }
        let title: String = title.chars().take(300).collect();
        let subtitle: String = req.subtitle.trim().chars().take(200).collect();
        let font = FontVec::try_from_vec(self.font_bytes.clone().expect("checked")).map_err(|_| ImageError::NotSetUp("no image generator is set up: the font file could not be read".into()))?;
        let (w, h) = (req.width, req.height);
        let (c1, c2, text, accent) = palette(&req.style);

        // Diagonal gradient.
        let mut img = RgbImage::new(w, h);
        let denom = (w + h) as f32;
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = mix(c1, c2, (x + y) as f32 / denom);
        }

        let margin = (w.min(h) as f32 * 0.09).max(12.0);
        let max_w = w as f32 - 2.0 * margin;
        // Largest headline size that fits in the height available.
        let sub_scale = |title_px: f32| PxScale::from((title_px * 0.42).max(12.0));
        let mut px = (h as f32 * 0.16).min(w as f32 * 0.12).max(16.0);
        let (title_lines, title_scale) = loop {
            let scale = PxScale::from(px);
            let lines = wrap(&font, scale, &title, max_w);
            let line_h = px * 1.18;
            let sub_h = if subtitle.is_empty() { 0.0 } else { sub_scale(px).y * 1.4 * wrap(&font, sub_scale(px), &subtitle, max_w).len() as f32 + px * 0.35 };
            if (lines.len() as f32 * line_h + sub_h) <= h as f32 - 2.0 * margin || px <= 16.0 {
                break (lines, scale);
            }
            px = (px * 0.9).max(16.0);
        };
        let line_h = title_scale.y * 1.18;
        let sub_lines = if subtitle.is_empty() { vec![] } else { wrap(&font, sub_scale(title_scale.y), &subtitle, max_w) };
        let block_h = title_lines.len() as f32 * line_h + if sub_lines.is_empty() { 0.0 } else { title_scale.y * 0.35 + sub_lines.len() as f32 * sub_scale(title_scale.y).y * 1.4 };
        let mut y = ((h as f32 - block_h) / 2.0).max(margin);

        // A thin accent bar above the headline.
        let bar_y = (y - title_scale.y * 0.25).max(margin * 0.5) as u32;
        for x in margin as u32..(margin + (w as f32 * 0.12)).min(w as f32 - margin) as u32 {
            for dy in 0..(h / 90).max(3) {
                if bar_y + dy < h {
                    img.put_pixel(x, bar_y + dy, Rgb(accent));
                }
            }
        }
        let ascent = font.as_scaled(title_scale).ascent();
        for line in &title_lines {
            draw_line(&mut img, &font, title_scale, line, margin, y + ascent, text);
            y += line_h;
        }
        if !sub_lines.is_empty() {
            y += title_scale.y * 0.35;
            let ss = sub_scale(title_scale.y);
            let sub_ascent = font.as_scaled(ss).ascent();
            for line in &sub_lines {
                draw_line(&mut img, &font, ss, line, margin, y + sub_ascent, accent);
                y += ss.y * 1.4;
            }
        }

        std::fs::create_dir_all(dir).map_err(|e| ImageError::Failed { message: format!("could not create the images folder: {e}"), retryable: false })?;
        let path = dir.join(format!("{}.png", super::safe_stem(stem)));
        img.save_with_format(&path, image::ImageFormat::Png).map_err(|e| ImageError::Failed { message: format!("could not save the image: {e}"), retryable: false })?;
        let alt = if subtitle.is_empty() { format!("A graphic reading: {title}") } else { format!("A graphic reading: {title} - {subtitle}") };
        Ok(ImageResult { path, width: w, height: h, alt_text: alt, provider: "card" })
    }
}
