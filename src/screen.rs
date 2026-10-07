//! Finds the LCD in a camera frame and returns it front-on.
//!
//! A port of mcp-camera4screen's detector: candidate quadrilaterals come from
//! edge contours, bright/dark Otsu masks, the "hole in a dark bezel" mask and
//! the rotated bounding box of bright UI content; each is scored by aspect
//! ratio, size and how much of the bright content it contains, gated by a
//! real brightness step along its border. Detection runs on a downscaled
//! copy (fast on a Pi Zero); the perspective warp samples the full frame.

use anyhow::{Result, anyhow, bail};
use image::{GrayImage, Luma, Rgb, RgbImage, imageops};
use imageproc::contours::{BorderType, find_contours};
use imageproc::contrast::{ThresholdType, otsu_level, threshold};
use imageproc::distance_transform::Norm;
use imageproc::drawing::{draw_filled_circle_mut, draw_hollow_polygon_mut, draw_polygon_mut};
use imageproc::edges::canny;
use imageproc::filter::{gaussian_blur_f32, median_filter};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use imageproc::geometry::{approximate_polygon_dp, arc_length, convex_hull, min_area_rect};
use imageproc::morphology::{close, dilate, erode};
use imageproc::point::Point;
use imageproc::region_labelling::{Connectivity, connected_components};

pub type Pt = (f32, f32);

/// Four corners ordered top-left, top-right, bottom-right, bottom-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad(pub [Pt; 4]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Auto,
    Manual,
    Cached,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub min_area_ratio: f32,
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub margin_ratio: f32,
    pub rotation_degrees: i32,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
}

/// Longest side of the image used for detection.
const DETECT_SIZE: u32 = 480;

// ---------------------------------------------------------------- geometry

fn dist(a: Pt, b: Pt) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn polygon_area(pts: &[Pt]) -> f32 {
    let n = pts.len();
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        s += a.0 * b.1 - b.0 * a.1;
    }
    (s / 2.0).abs()
}

fn is_convex(pts: &[Pt]) -> bool {
    let n = pts.len();
    let mut sign = 0.0f32;
    for i in 0..n {
        let (a, b, c) = (pts[i], pts[(i + 1) % n], pts[(i + 2) % n]);
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if cross.abs() < 1e-3 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

impl Quad {
    /// Orders arbitrary four points as TL, TR, BR, BL: sort them clockwise
    /// around the centroid, then start from the one nearest the top-left.
    pub fn ordered(pts: [Pt; 4]) -> Quad {
        let cx = pts.iter().map(|p| p.0).sum::<f32>() / 4.0;
        let cy = pts.iter().map(|p| p.1).sum::<f32>() / 4.0;
        let mut v = pts;
        v.sort_by(|a, b| {
            (a.1 - cy)
                .atan2(a.0 - cx)
                .total_cmp(&(b.1 - cy).atan2(b.0 - cx))
        });
        let start = (0..4)
            .min_by(|&i, &j| (v[i].0 + v[i].1).total_cmp(&(v[j].0 + v[j].1)))
            .unwrap();
        Quad([
            v[start],
            v[(start + 1) % 4],
            v[(start + 2) % 4],
            v[(start + 3) % 4],
        ])
    }

    pub fn area(&self) -> f32 {
        polygon_area(&self.0)
    }

    fn centroid(&self) -> Pt {
        let s = self
            .0
            .iter()
            .fold((0.0, 0.0), |acc, p| (acc.0 + p.0, acc.1 + p.1));
        (s.0 / 4.0, s.1 / 4.0)
    }

    fn scaled(&self, f: f32) -> Quad {
        Quad(self.0.map(|p| (p.0 * f, p.1 * f)))
    }

    /// Shrinks (positive) or grows (negative) the quad toward its centroid.
    fn with_margin(&self, margin_ratio: f32) -> Quad {
        if margin_ratio == 0.0 {
            return *self;
        }
        let c = self.centroid();
        let f = 1.0 - margin_ratio;
        Quad(
            self.0
                .map(|p| (c.0 + (p.0 - c.0) * f, c.1 + (p.1 - c.1) * f)),
        )
    }

    /// Natural output size: the longer of each pair of opposite sides.
    fn output_size(&self) -> (u32, u32) {
        let [tl, tr, br, bl] = self.0;
        let w = dist(br, bl).max(dist(tr, tl)).round().max(1.0);
        let h = dist(tr, br).max(dist(tl, bl)).round().max(1.0);
        (w as u32, h as u32)
    }

    fn to_i32(self) -> Vec<Point<i32>> {
        self.0
            .iter()
            .map(|p| Point::new(p.0.round() as i32, p.1.round() as i32))
            .collect()
    }
}

// ---------------------------------------------------------------- candidates

fn luma_of(rgb: &RgbImage) -> GrayImage {
    imageops::grayscale(rgb)
}

/// HSV value channel: max(r, g, b). Keeps coloured content that luminance drops.
fn value_of(rgb: &RgbImage) -> GrayImage {
    let mut v = GrayImage::new(rgb.width(), rgb.height());
    for (p, q) in rgb.pixels().zip(v.pixels_mut()) {
        *q = Luma([p[0].max(p[1]).max(p[2])]);
    }
    v
}

fn median(img: &GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for p in img.pixels() {
        hist[p[0] as usize] += 1;
    }
    let half = (img.width() * img.height()).div_ceil(2);
    let mut acc = 0;
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if acc >= half {
            return i as u8;
        }
    }
    255
}

fn percentile(img: &GrayImage, pct: f32) -> u8 {
    let mut hist = [0u32; 256];
    for p in img.pixels() {
        hist[p[0] as usize] += 1;
    }
    let target = ((img.width() * img.height()) as f32 * pct / 100.0) as u32;
    let mut acc = 0;
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if acc >= target {
            return i as u8;
        }
    }
    255
}

fn invert(img: &GrayImage) -> GrayImage {
    let mut out = img.clone();
    for p in out.pixels_mut() {
        p[0] = 255 - p[0];
    }
    out
}

/// Up to two convex quads per contour: from the contour itself and from its hull.
fn quads_from_points(points: &[Point<i32>]) -> Vec<Quad> {
    let mut out = Vec::new();
    let hull = convex_hull(points.to_vec());
    for target in [points, hull.as_slice()] {
        if target.len() < 4 {
            continue;
        }
        let peri = arc_length(target, true);
        for eps in [0.02, 0.03, 0.05, 0.08, 0.12, 0.18] {
            let approx = approximate_polygon_dp(target, eps * peri, true);
            if approx.len() == 4 {
                let pts: Vec<Pt> = approx.iter().map(|p| (p.x as f32, p.y as f32)).collect();
                let shortest = (0..4)
                    .map(|i| dist(pts[i], pts[(i + 1) % 4]))
                    .fold(f32::MAX, f32::min);
                if shortest >= 4.0 && is_convex(&pts) {
                    out.push(Quad::ordered([pts[0], pts[1], pts[2], pts[3]]));
                    break;
                }
            }
        }
    }
    out
}

fn candidates_from_mask(mask: &GrayImage, min_area: f32) -> Vec<Quad> {
    let mut out = Vec::new();
    for c in find_contours::<i32>(mask) {
        if c.border_type != BorderType::Outer || c.points.len() < 4 {
            continue;
        }
        let pts: Vec<Pt> = c.points.iter().map(|p| (p.x as f32, p.y as f32)).collect();
        if polygon_area(&pts) < min_area {
            continue;
        }
        out.extend(quads_from_points(&c.points));
    }
    out
}

/// Mask of the brightest on-screen content: high-percentile threshold, opening,
/// and a size filter against specular highlights and indicator LEDs.
fn bright_content_mask(value: &GrayImage) -> GrayImage {
    // A high percentile, but on a mostly-dark frame with sparse content it
    // lands above the dimmer UI elements, so cap it relative to the dark mass.
    let (p50, p80, p92) = (
        percentile(value, 50.0),
        percentile(value, 80.0),
        percentile(value, 92.0),
    );
    let mut t = if p92 >= 250 { p80 } else { p92 };
    t = t.min((p80 as u32 + 10).max(2 * p50 as u32).min(255) as u8);
    // `threshold` keeps pixels strictly above t; a percentile that lands on
    // flat bright content (a white card) would otherwise exclude it.
    t = t.saturating_sub(1);
    #[cfg(test)]
    eprintln!("  content threshold t={t} (p50={p50} p80={p80} p92={p92})");
    let mask = threshold(value, t, ThresholdType::Binary);
    let labels = connected_components(&mask, Connectivity::Eight, Luma([0u8]));
    let n = labels.pixels().map(|p| p[0]).max().unwrap_or(0) as usize;
    if n == 0 {
        return GrayImage::new(mask.width(), mask.height());
    }
    let mut sizes = vec![0u32; n + 1];
    for p in labels.pixels() {
        sizes[p[0] as usize] += 1;
    }
    let min_cc = 50.max((mask.width() * mask.height()) / 100_000);
    let mut keep = GrayImage::new(mask.width(), mask.height());
    for (p, q) in labels.pixels().zip(keep.pixels_mut()) {
        let l = p[0] as usize;
        if l != 0 && sizes[l] >= min_cc {
            q[0] = 255;
        }
    }
    keep
}

fn ui_bright_quad(content: &GrayImage) -> Option<Quad> {
    let (w, h) = (content.width() as f32, content.height() as f32);
    let pts: Vec<Point<i32>> = content
        .enumerate_pixels()
        .filter(|(_, _, p)| p[0] > 0)
        .map(|(x, y, _)| Point::new(x as i32, y as i32))
        .collect();
    if pts.len() < 64 {
        return None;
    }
    let rect = min_area_rect(&pts);
    let clip = |p: &Point<i32>| {
        (
            (p.x as f32).clamp(0.0, w - 1.0),
            (p.y as f32).clamp(0.0, h - 1.0),
        )
    };
    Some(Quad::ordered([
        clip(&rect[0]),
        clip(&rect[1]),
        clip(&rect[2]),
        clip(&rect[3]),
    ]))
}

// ---------------------------------------------------------------- straight edges

/// Quads spanned by the longest straight edges (Hough transform on the edge
/// map): an LCD's four borders are long straight lines even when the panel
/// and its bezel are both dark and no brightness mask separates them, and
/// even when the edge is faint and broken, since votes add up along the line.
fn line_rect_candidates(edges: &GrayImage, min_area: f32) -> Vec<Quad> {
    use imageproc::hough::{LineDetectionOptions, detect_lines};
    let (w, h) = (edges.width() as f32, edges.height() as f32);
    let short = w.min(h);
    // Try strict first; relax until each orientation has at least two lines.
    let mut horizontals: Vec<(f32, f32, f32)> = Vec::new();
    let mut verticals: Vec<(f32, f32, f32)> = Vec::new();
    for frac in [0.5, 0.35, 0.25, 0.18] {
        let lines = detect_lines(
            edges,
            LineDetectionOptions {
                vote_threshold: (short * frac) as u32,
                suppression_radius: 10,
            },
        );
        horizontals.clear();
        verticals.clear();
        for l in lines {
            let th = (l.angle_in_degrees as f32).to_radians();
            let (a, b, c) = (th.cos(), th.sin(), -l.r);
            let deg = l.angle_in_degrees;
            if (75..=105).contains(&deg) {
                horizontals.push((a, b, c));
            } else if deg <= 15 || deg >= 165 {
                verticals.push((a, b, c));
            }
        }
        if horizontals.len() >= 2 && verticals.len() >= 2 {
            break;
        }
    }
    horizontals.truncate(5);
    verticals.truncate(5);
    #[cfg(test)]
    eprintln!(
        "  hough: horizontal {:?} vertical {:?}",
        horizontals
            .iter()
            .map(|l| (-l.2) as i32)
            .collect::<Vec<_>>(),
        verticals.iter().map(|l| (-l.2) as i32).collect::<Vec<_>>()
    );
    let intersect = |l1: (f32, f32, f32), l2: (f32, f32, f32)| -> Option<Pt> {
        let det = l1.0 * l2.1 - l2.0 * l1.1;
        if det.abs() < 1e-6 {
            return None;
        }
        let x = (l1.1 * l2.2 - l2.1 * l1.2) / det;
        let y = (l2.0 * l1.2 - l1.0 * l2.2) / det;
        let slack = 0.02 * short;
        ((-slack..=w - 1.0 + slack).contains(&x) && (-slack..=h - 1.0 + slack).contains(&y))
            .then_some((x.clamp(0.0, w - 1.0), y.clamp(0.0, h - 1.0)))
    };
    let mut out = Vec::new();
    for (i, &h1) in horizontals.iter().enumerate() {
        for &h2 in &horizontals[i + 1..] {
            for (j, &v1) in verticals.iter().enumerate() {
                for &v2 in &verticals[j + 1..] {
                    let (Some(p1), Some(p2), Some(p3), Some(p4)) = (
                        intersect(h1, v1),
                        intersect(h1, v2),
                        intersect(h2, v2),
                        intersect(h2, v1),
                    ) else {
                        continue;
                    };
                    let q = Quad::ordered([p1, p2, p3, p4]);
                    let shortest = (0..4)
                        .map(|k| dist(q.0[k], q.0[(k + 1) % 4]))
                        .fold(f32::MAX, f32::min);
                    if shortest >= 0.15 * short && q.area() >= min_area && is_convex(&q.0) {
                        out.push(q);
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- refinement

/// Snaps each side of a quad to the edge pixels in a strip around it with an
/// orthogonal least-squares fit, so corners found from blobby masks land on
/// the real (possibly keystoned) border.  A side with too few edge pixels in
/// its strip keeps its original line.
fn refine_with_edges(edge_pts: &[(f32, f32)], q: &Quad, band: f32, w: f32, h: f32) -> Quad {
    let mut lines = [(0.0f32, 0.0f32, 0.0f32); 4];
    #[allow(clippy::needless_range_loop)]
    for i in 0..4 {
        let (p, r) = (q.0[i], q.0[(i + 1) % 4]);
        let len = dist(p, r).max(1.0);
        let (dx, dy) = ((r.0 - p.0) / len, (r.1 - p.1) / len);
        let (nx, ny) = (-dy, dx);
        let fallback = (nx, ny, -(nx * p.0 + ny * p.1));
        let (mut n, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32);
        let mut pts: Vec<(f32, f32)> = Vec::new();
        for &(x, y) in edge_pts {
            let (vx, vy) = (x - p.0, y - p.1);
            let along = vx * dx + vy * dy;
            if along < -band || along > len + band {
                continue;
            }
            let across = vx * nx + vy * ny;
            if across.abs() <= band {
                pts.push((x, y));
                n += 1.0;
                sx += x;
                sy += y;
            }
        }
        lines[i] = if n < 0.3 * len {
            fallback
        } else {
            let (mx, my) = (sx / n, sy / n);
            let (mut sxx, mut sxy, mut syy) = (0.0f32, 0.0f32, 0.0f32);
            for (x, y) in &pts {
                sxx += (x - mx) * (x - mx);
                sxy += (x - mx) * (y - my);
                syy += (y - my) * (y - my);
            }
            // principal direction of the strip's edge pixels
            let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
            let (ux, uy) = (theta.cos(), theta.sin());
            let (a, b) = (-uy, ux);
            if (a * nx + b * ny).abs() < 0.9 {
                fallback // the fit wandered off the side's direction
            } else {
                (a, b, -(a * mx + b * my))
            }
        };
    }
    let mut corners = [(0.0f32, 0.0f32); 4];
    for i in 0..4 {
        let (a1, b1, c1) = lines[(i + 3) % 4];
        let (a2, b2, c2) = lines[i];
        let det = a1 * b2 - a2 * b1;
        if det.abs() < 1e-6 {
            return *q;
        }
        let x = (b1 * c2 - b2 * c1) / det;
        let y = (a2 * c1 - a1 * c2) / det;
        if !(-band..=w - 1.0 + band).contains(&x) || !(-band..=h - 1.0 + band).contains(&y) {
            return *q;
        }
        corners[i] = (x.clamp(0.0, w - 1.0), y.clamp(0.0, h - 1.0));
    }
    let refined = Quad::ordered(corners);
    if is_convex(&refined.0) && (refined.area() - q.area()).abs() < 0.25 * q.area() {
        refined
    } else {
        *q
    }
}

// ---------------------------------------------------------------- scoring

const COMMON_ASPECTS: [f32; 7] = [
    16.0 / 9.0,
    16.0 / 10.0,
    4.0 / 3.0,
    5.0 / 4.0,
    5.0 / 3.0,
    3.0 / 2.0,
    21.0 / 9.0,
];

fn aspect_score(q: &Quad) -> f32 {
    let [tl, tr, br, bl] = q.0;
    let w = (dist(tl, tr) + dist(bl, br)) / 2.0;
    let h = (dist(tr, br) + dist(tl, bl)) / 2.0;
    if w < 1.0 || h < 1.0 {
        return 0.0;
    }
    let ratio = w.max(h) / w.min(h);
    let best = COMMON_ASPECTS
        .iter()
        .map(|r| (ratio - r).abs() / r)
        .fold(f32::MAX, f32::min);
    (1.0 - best).max(0.0)
}

fn filled_mask(w: u32, h: u32, q: &Quad) -> GrayImage {
    let mut m = GrayImage::new(w, h);
    let pts = q.to_i32();
    // draw_polygon_mut panics when the first and last points coincide.
    if pts.first() != pts.last() {
        draw_polygon_mut(&mut m, &pts, Luma([255u8]));
    }
    m
}

fn mean_under(gray: &GrayImage, mask: &GrayImage) -> Option<f32> {
    let (mut sum, mut n) = (0u64, 0u64);
    for (g, m) in gray.pixels().zip(mask.pixels()) {
        if m[0] > 0 {
            sum += g[0] as u64;
            n += 1;
        }
    }
    (n > 0).then(|| sum as f32 / n as f32)
}

/// (contrast, outside_uniformity): how real the quad's boundary is, as the
/// brightness step between a thin ring just inside and just outside it
/// (saturating at ~20 grey levels), and how uniform the outside ring is.
/// An LCD's edge is bordered by its bezel, which is plain; the bezel's own
/// outer edge is bordered by the board or desk, which is cluttered. That
/// tells the two apart when both have a crisp step.
fn border_contrast(gray: &GrayImage, q: &Quad, band: u8) -> (f32, f32) {
    // Leave a small gap between the quad's edge and both rings so that a
    // candidate that sits a pixel or two off the real edge is not judged by
    // the anti-aliased edge pixels themselves.
    const GAP: u8 = 2;
    let filled = filled_mask(gray.width(), gray.height(), q);
    let inner_a = erode(&filled, Norm::LInf, GAP);
    let inner_b = erode(&filled, Norm::LInf, band + GAP);
    let outer_a = dilate(&filled, Norm::LInf, GAP);
    let outer_b = dilate(&filled, Norm::LInf, band + GAP);
    let ring = |big: &GrayImage, small: &GrayImage| {
        let mut m = big.clone();
        for (p, s) in m.pixels_mut().zip(small.pixels()) {
            if s[0] > 0 {
                p[0] = 0;
            }
        }
        m
    };
    let inside = ring(&inner_a, &inner_b);
    let outside = ring(&outer_b, &outer_a);
    let (Some(a), Some(b)) = (mean_under(gray, &inside), mean_under(gray, &outside)) else {
        return (0.0, 0.0);
    };
    let contrast = ((a - b).abs() / 255.0 / 0.08).min(1.0);
    let (mut sum2, mut n) = (0.0f64, 0u32);
    for (g, m) in gray.pixels().zip(outside.pixels()) {
        if m[0] > 0 {
            sum2 += (g[0] as f64 - b as f64).powi(2);
            n += 1;
        }
    }
    let std = (sum2 / n.max(1) as f64).sqrt() as f32;
    (contrast, (1.0 - std / 40.0).clamp(0.0, 1.0))
}

/// True when the quad traces the whole camera view rather than a framed screen.
fn is_frame_quad(q: &Quad, w: u32, h: u32) -> bool {
    let (wf, hf) = (w as f32 - 1.0, h as f32 - 1.0);
    let tol = 0.015 * wf.max(hf);
    let corners = [(0.0, 0.0), (wf, 0.0), (wf, hf), (0.0, hf)];
    let matched = corners
        .iter()
        .filter(|c| q.0.iter().any(|p| dist(*p, **c) < tol))
        .count();
    if matched >= 3 {
        return true;
    }
    let xs = q.0.iter().map(|p| p.0);
    let ys = q.0.iter().map(|p| p.1);
    let span_x = xs.clone().fold(f32::MIN, f32::max) - xs.fold(f32::MAX, f32::min);
    let span_y = ys.clone().fold(f32::MIN, f32::max) - ys.fold(f32::MAX, f32::min);
    span_x >= 0.96 * wf && span_y >= 0.96 * hf
}

fn content_coverage(content: &GrayImage, q: &Quad, total: u32) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let m = filled_mask(content.width(), content.height(), q);
    let inside = content
        .pixels()
        .zip(m.pixels())
        .filter(|(c, m)| c[0] > 0 && m[0] > 0)
        .count();
    inside as f32 / total as f32
}

#[cfg(test)]
thread_local! {
    static DEBUG_DIR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn debug_save(name: &str, img: &GrayImage) {
    DEBUG_DIR.with(|d| {
        if let Some(dir) = d.borrow().as_ref() {
            let _ = img.save(format!("{dir}/{name}.png"));
        }
    });
}
#[cfg(not(test))]
fn debug_save(_name: &str, _img: &GrayImage) {}

/// The LCD-likeliest convex quad in a (small) frame, or None.
pub fn find_screen_quad(rgb: &RgbImage, min_area_ratio: f32) -> Option<Quad> {
    let (w, h) = rgb.dimensions();
    let gray = luma_of(rgb);
    let value = value_of(rgb);
    let smooth = gaussian_blur_f32(&gray, 1.2);
    let smooth_v = median_filter(&value, 1, 1);

    // Canny thresholds scale with the image's median (bright scenes have
    // bigger steps) but are clamped to what a 20..70-level step produces in
    // imageproc's Sobel-magnitude units, so a panel edge inside a bezel is
    // never thresholded away on a bright desk.
    let med = median(&smooth) as f32;
    let hi = (1.33 * med).clamp(25.0, 90.0);
    let edges = close(&canny(&smooth, hi * 0.4, hi), Norm::LInf, 1);
    let bright = close(
        &threshold(&smooth_v, otsu_level(&smooth_v), ThresholdType::Binary),
        Norm::LInf,
        2,
    );
    let dark = invert(&bright);
    let bezel_hole = invert(&close(
        &threshold(&smooth_v, 50, ThresholdType::BinaryInverted),
        Norm::LInf,
        3,
    ));

    let p50 = percentile(&smooth_v, 50.0) as u32;
    let glow_masks: Vec<GrayImage> = [p50 * 3 / 2 + 4, p50 * 2 + 8, p50 * 3 + 8]
        .into_iter()
        .filter(|t| *t < 250)
        .map(|t| {
            close(
                &threshold(&smooth_v, t as u8, ThresholdType::Binary),
                Norm::LInf,
                2,
            )
        })
        .collect();
    let content = bright_content_mask(&smooth_v);
    let total_bright = content.pixels().filter(|p| p[0] > 0).count() as u32;
    debug_save("gray", &gray);
    debug_save("edges", &edges);
    debug_save("bright", &bright);
    debug_save("bezel_hole", &bezel_hole);
    debug_save("content", &content);
    for (i, m) in glow_masks.iter().enumerate() {
        debug_save(&format!("glow{i}"), m);
    }

    let frame_area = (w * h) as f32;
    let min_area = frame_area * min_area_ratio;
    let band = (0.012 * w.max(h) as f32).round().max(4.0) as u8;
    let use_coverage = total_bright as f32 >= 0.005 * frame_area;

    let mut pool: Vec<(&str, Quad)> = Vec::new();
    pool.extend(
        line_rect_candidates(&edges, min_area)
            .into_iter()
            .map(|q| ("lines", q)),
    );
    pool.extend(
        candidates_from_mask(&edges, min_area)
            .into_iter()
            .map(|q| ("edges", q)),
    );
    pool.extend(
        candidates_from_mask(&bright, min_area)
            .into_iter()
            .map(|q| ("bright", q)),
    );
    pool.extend(
        candidates_from_mask(&dark, min_area)
            .into_iter()
            .map(|q| ("dark", q)),
    );
    pool.extend(
        candidates_from_mask(&bezel_hole, min_area)
            .into_iter()
            .map(|q| ("bezel", q)),
    );
    for m in &glow_masks {
        pool.extend(
            candidates_from_mask(m, min_area)
                .into_iter()
                .map(|q| ("glow", q)),
        );
    }
    if let Some(q) = ui_bright_quad(&content)
        && q.area() >= min_area
    {
        pool.push(("ui", q));
    }

    // Tightness: how much of the quad's bounding box the bright content's own
    // bounding box fills.  The LCD edge hugs the UI; the bezel's outer edge
    // adds the bezel width all round and scores lower.
    let content_bbox = {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for (x, y, p) in content.enumerate_pixels() {
            if p[0] > 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        (x0 <= x1 && y0 <= y1).then_some(((x1 - x0 + 1) * (y1 - y0 + 1)) as f32)
    };
    let tightness = |q: &Quad| -> f32 {
        let Some(cb) = content_bbox else { return 0.5 };
        let xs = q.0.iter().map(|p| p.0);
        let ys = q.0.iter().map(|p| p.1);
        let bw = xs.clone().fold(f32::MIN, f32::max) - xs.fold(f32::MAX, f32::min);
        let bh = ys.clone().fold(f32::MIN, f32::max) - ys.fold(f32::MAX, f32::min);
        (cb / (bw * bh).max(1.0)).min(1.0)
    };

    // Two rankings: boundary-backed quads that contain (nearly) all the
    // content, and everything else.  The content bounding box ("ui") is a
    // last resort: it is always tight and always covers everything, so it
    // only competes when no real boundary candidate does.
    let edge_pts: Vec<(f32, f32)> = edges
        .enumerate_pixels()
        .filter(|(_, _, p)| p[0] > 0)
        .map(|(x, y, _)| (x as f32, y as f32))
        .collect();
    let mut seen: Vec<Quad> = Vec::new();
    let mut best_real: Option<(f32, Quad)> = None;
    let mut best_any: Option<(f32, Quad)> = None;
    for (origin, q) in pool {
        let q = if origin == "ui" {
            q
        } else {
            refine_with_edges(&edge_pts, &q, band as f32, w as f32, h as f32)
        };
        if seen
            .iter()
            .any(|s| s.0.iter().zip(q.0.iter()).all(|(a, b)| dist(*a, *b) < 2.0))
        {
            continue;
        }
        seen.push(q);
        if is_frame_quad(&q, w, h) {
            #[cfg(test)]
            eprintln!(
                "  {origin:6} frame-quad {:?}",
                q.0.map(|p| (p.0 as i32, p.1 as i32))
            );
            continue;
        }
        let (mut boundary, outside_plain) = border_contrast(&gray, &q, band);
        if origin == "ui" {
            // Bounded by the content rather than by a bezel step.
            boundary = boundary.max(0.5);
        }
        let area_ratio = (q.area() / frame_area).min(0.90);
        let aspect = aspect_score(&q);
        let coverage = content_coverage(&content, &q, total_bright);
        let tight = tightness(&q);
        let score = if use_coverage {
            // A quad that leaves bright content outside is a sub-element
            // (a card, a panel of a multi-panel UI), not the screen.
            let sub_element = if coverage >= 0.9 {
                1.0
            } else if coverage >= 0.75 {
                0.6 + (coverage - 0.75) / 0.15 * 0.4
            } else {
                0.4
            };
            sub_element
                * boundary
                * (0.30 * aspect
                    + 0.05 * area_ratio
                    + 0.30 * coverage
                    + 0.15 * outside_plain
                    + 0.20 * tight)
        } else {
            boundary * (0.45 * aspect + 0.35 * area_ratio + 0.20 * outside_plain)
        };
        #[cfg(test)]
        eprintln!(
            "  {origin:6} score={score:.3} boundary={boundary:.2} plain={outside_plain:.2} tight={tight:.2} aspect={aspect:.2} area={area_ratio:.2} cov={coverage:.2} {:?}",
            q.0.map(|p| (p.0 as i32, p.1 as i32))
        );
        if boundary <= 0.0 || score <= 0.0 {
            continue;
        }
        if origin != "ui"
            && (!use_coverage || coverage >= 0.9)
            && best_real.is_none_or(|(s, _)| score > s)
        {
            best_real = Some((score, q));
        }
        if best_any.is_none_or(|(s, _)| score > s) {
            best_any = Some((score, q));
        }
    }
    best_real.or(best_any).map(|(_, q)| q)
}

/// A capped or unplugged sensor: near-uniform and dim.
pub fn looks_blanked(gray: &GrayImage) -> bool {
    let n = (gray.width() * gray.height()) as f64;
    let mean = gray.pixels().map(|p| p[0] as f64).sum::<f64>() / n;
    let var = gray
        .pixels()
        .map(|p| (p[0] as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    var.sqrt() < 5.0 && mean < 50.0
}

// ---------------------------------------------------------------- public API

fn small_copy(frame: &RgbImage) -> (RgbImage, f32) {
    let long = frame.width().max(frame.height());
    if long <= DETECT_SIZE {
        return (frame.clone(), 1.0);
    }
    let f = DETECT_SIZE as f32 / long as f32;
    let (w, h) = (
        (frame.width() as f32 * f).round() as u32,
        (frame.height() as f32 * f).round() as u32,
    );
    (
        imageops::resize(frame, w.max(1), h.max(1), imageops::FilterType::Triangle),
        f,
    )
}

/// Finds the screen in a full-resolution frame; corners are in frame coordinates.
pub fn detect(frame: &RgbImage, min_area_ratio: f32) -> Result<Quad> {
    let (small, f) = small_copy(frame);
    if looks_blanked(&luma_of(&small)) {
        bail!("frame appears blanked (lens cap, no light, or no signal)");
    }
    let q = find_screen_quad(&small, min_area_ratio)
        .ok_or_else(|| anyhow!("no screen-like quadrilateral found; frame the LCD so it dominates the view, add light, or pass manual_corners"))?;
    Ok(q.scaled(1.0 / f))
}

/// Perspective-corrects the quad out of the frame and applies the output options.
pub fn rectify(frame: &RgbImage, corners: Quad, opts: &Options) -> RgbImage {
    let corners = corners.with_margin(opts.margin_ratio);
    let (nat_w, nat_h) = corners.output_size();
    let (out_w, out_h) = match (opts.output_width, opts.output_height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (
            w,
            ((nat_h as f32 * w as f32 / nat_w as f32).round() as u32).max(1),
        ),
        (None, Some(h)) => (
            ((nat_w as f32 * h as f32 / nat_h as f32).round() as u32).max(1),
            h,
        ),
        (None, None) => (nat_w, nat_h),
    };
    let dst = [
        (0.0, 0.0),
        (out_w as f32 - 1.0, 0.0),
        (out_w as f32 - 1.0, out_h as f32 - 1.0),
        (0.0, out_h as f32 - 1.0),
    ];
    let mut out = RgbImage::new(out_w, out_h);
    if let Some(p) = Projection::from_control_points(corners.0, dst) {
        warp_into(
            frame,
            p,
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
            &mut out,
        );
    }
    if opts.flip_horizontal {
        out = imageops::flip_horizontal(&out);
    }
    if opts.flip_vertical {
        out = imageops::flip_vertical(&out);
    }
    match opts.rotation_degrees.rem_euclid(360) {
        90 => imageops::rotate90(&out),
        180 => imageops::rotate180(&out),
        270 => imageops::rotate270(&out),
        _ => out,
    }
}

/// Draws the corners on a copy of the frame: polygon, a big dot on TL, small dots elsewhere.
pub fn annotate(frame: &RgbImage, corners: &Quad, source: Source) -> RgbImage {
    let mut out = frame.clone();
    let color = match source {
        Source::Manual => Rgb([255, 0, 255]),
        _ => Rgb([0, 255, 0]),
    };
    let r = (frame.width().max(frame.height()) as f32 * 0.006).max(3.0) as i32;
    let pts: Vec<Point<f32>> = corners.0.iter().map(|p| Point::new(p.0, p.1)).collect();
    for d in -1..=1 {
        let shifted: Vec<Point<f32>> = pts
            .iter()
            .map(|p| Point::new(p.x + d as f32, p.y))
            .collect();
        draw_hollow_polygon_mut(&mut out, &shifted, color);
    }
    for (i, p) in corners.0.iter().enumerate() {
        let radius = if i == 0 { r * 2 } else { r };
        draw_filled_circle_mut(
            &mut out,
            (p.0.round() as i32, p.1.round() as i32),
            radius,
            Rgb([255, 0, 0]),
        );
    }
    out
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.to_rgb8())
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((img.width() * img.height() / 4) as usize);
    jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100)).encode(
        img.as_raw(),
        u16::try_from(img.width())?,
        u16::try_from(img.height())?,
        jpeg_encoder::ColorType::Rgb,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> RgbImage {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        image::open(format!("{path}{name}")).unwrap().to_rgb8()
    }

    #[test]
    fn ordering_and_area() {
        let q = Quad::ordered([(10.0, 0.0), (0.0, 10.0), (0.0, 0.0), (10.0, 10.0)]);
        assert_eq!(q.0, [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]);
        assert_eq!(q.area(), 100.0);
        assert_eq!(q.output_size(), (10, 10));
        assert_eq!(q.with_margin(0.5).0[0], (2.5, 2.5));
        assert!(is_convex(&q.0));
        assert!(!is_convex(&[
            (0.0, 0.0),
            (10.0, 10.0),
            (10.0, 0.0),
            (0.0, 10.0)
        ]));
        assert!(is_frame_quad(
            &Quad::ordered([(0.0, 0.0), (99.0, 0.0), (99.0, 99.0), (0.0, 99.0)]),
            100,
            100
        ));
    }

    #[test]
    fn rectify_identity_on_axis_aligned_quad() {
        let mut img = RgbImage::new(100, 60);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgb([x as u8, y as u8, 7]);
        }
        let q = Quad([(10.0, 10.0), (49.0, 10.0), (49.0, 29.0), (10.0, 29.0)]);
        let out = rectify(&img, q, &Options::default());
        assert_eq!(out.dimensions(), (39, 19));
        assert_eq!(out.get_pixel(0, 0)[0], 10);
        assert_eq!(out.get_pixel(38, 18)[1], 29);
        let rotated = rectify(
            &img,
            q,
            &Options {
                rotation_degrees: 90,
                ..Default::default()
            },
        );
        assert_eq!(rotated.dimensions(), (19, 39));
        let forced = rectify(
            &img,
            q,
            &Options {
                output_width: Some(78),
                ..Default::default()
            },
        );
        assert_eq!(forced.dimensions(), (78, 38));
    }

    /// Rasterises a quad into a mask and returns intersection-over-union.
    fn iou(a: &Quad, b: &Quad, w: u32, h: u32) -> f32 {
        let ma = filled_mask(w, h, a);
        let mb = filled_mask(w, h, b);
        let (mut inter, mut union) = (0u32, 0u32);
        for (x, y) in ma.pixels().zip(mb.pixels()) {
            let (x, y) = (x[0] > 0, y[0] > 0);
            if x && y {
                inter += 1;
            }
            if x || y {
                union += 1;
            }
        }
        inter as f32 / union.max(1) as f32
    }

    /// A synthetic bench scene: textured desk, a bezel, and an LCD showing a
    /// few UI blocks, optionally rotated and keystoned.
    fn synthetic_scene(panel: Quad, panel_rgb: [u8; 3], bezel_rgb: [u8; 3], desk: u8) -> RgbImage {
        let (w, h) = (960u32, 720u32);
        let mut img = RgbImage::from_fn(w, h, |x, y| {
            let n = ((x * 7 + y * 13) % 23) as u8;
            Rgb([
                desk.saturating_add(n),
                desk.saturating_add(n / 2),
                desk.saturating_add(n),
            ])
        });
        let grow = panel.with_margin(-0.12);
        draw_polygon_mut(&mut img, &grow.to_i32(), Rgb(bezel_rgb));
        draw_polygon_mut(&mut img, &panel.to_i32(), Rgb(panel_rgb));
        // UI content: a title bar and three tiles, placed inside the panel.
        let inner = panel.with_margin(0.08);
        let [tl, tr, br, bl] = inner.0;
        let lerp = |a: Pt, b: Pt, t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        let bar = Quad([tl, tr, lerp(tr, br, 0.12), lerp(tl, bl, 0.12)]);
        draw_polygon_mut(&mut img, &bar.to_i32(), Rgb([40, 90, 200]));
        for i in 0..3 {
            let (u0, u1) = (0.05 + i as f32 * 0.32, 0.30 + i as f32 * 0.32);
            let top0 = lerp(lerp(tl, bl, 0.3), lerp(tr, br, 0.3), u0);
            let top1 = lerp(lerp(tl, bl, 0.3), lerp(tr, br, 0.3), u1);
            let bot1 = lerp(lerp(tl, bl, 0.8), lerp(tr, br, 0.8), u1);
            let bot0 = lerp(lerp(tl, bl, 0.8), lerp(tr, br, 0.8), u0);
            draw_polygon_mut(
                &mut img,
                &Quad([top0, top1, bot1, bot0]).to_i32(),
                Rgb([230, 230, 235]),
            );
        }
        img
    }

    #[test]
    fn detects_synthetic_panels() {
        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, Quad, [u8; 3], [u8; 3], u8)> = vec![
            // bright panel in a dark bezel on a light desk, straight on
            (
                "light-desk",
                Quad([
                    (220.0, 160.0),
                    (740.0, 160.0),
                    (740.0, 560.0),
                    (220.0, 560.0),
                ]),
                [120, 125, 130],
                [15, 15, 15],
                150,
            ),
            // dark UI panel, grey bezel, dark desk
            (
                "dark-ui",
                Quad([
                    (200.0, 140.0),
                    (760.0, 140.0),
                    (760.0, 580.0),
                    (200.0, 580.0),
                ]),
                [35, 38, 45],
                [110, 110, 110],
                30,
            ),
            // keystoned and rotated: camera off-axis
            (
                "keystone",
                Quad([
                    (250.0, 190.0),
                    (720.0, 150.0),
                    (760.0, 570.0),
                    (210.0, 530.0),
                ]),
                [90, 95, 100],
                [20, 20, 20],
                120,
            ),
        ];
        for (name, truth, panel_rgb, bezel_rgb, desk) in cases {
            let img = synthetic_scene(truth, panel_rgb, bezel_rgb, desk);
            let dir = format!(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/target/screen-tests/synthetic-{}"
                ),
                name
            );
            std::fs::create_dir_all(&dir).unwrap();
            let _ = img.save(format!("{dir}/scene.png"));
            DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
            eprintln!("== synthetic {name}");
            let q = detect(&img, 0.05).unwrap_or_else(|e| panic!("{name}: {e}"));
            let score = iou(&q, &truth, img.width(), img.height());
            if score < 0.85 {
                let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/");
                std::fs::create_dir_all(dir).unwrap();
                let _ = annotate(&img, &q, Source::Auto).save(format!("{dir}synthetic-{name}.png"));
            }
            assert!(
                score >= 0.85,
                "{name}: IoU {score:.2}, got {:?}",
                q.0.map(|p| (p.0 as i32, p.1 as i32))
            );
        }
    }

    #[test]
    fn detects_the_lcd_in_the_fixture_photos() {
        for name in ["scene01_dark_ui_slint.png", "scene02_dashboard_dark.png"] {
            let frame = fixture(name);
            let dir = format!(
                concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/{}"),
                name
            );
            std::fs::create_dir_all(&dir).unwrap();
            DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir));
            eprintln!("== {name}");
            let q = detect(&frame, 0.05).unwrap_or_else(|e| panic!("{name}: {e}"));
            let frame_area = (frame.width() * frame.height()) as f32;
            let ratio = q.area() / frame_area;
            assert!(
                (0.08..0.95).contains(&ratio),
                "{name}: implausible area ratio {ratio}"
            );
            assert!(is_convex(&q.0), "{name}: not convex");
            let out = rectify(
                &frame,
                q,
                &Options {
                    output_width: Some(640),
                    ..Default::default()
                },
            );
            let dbg = annotate(&frame, &q, Source::Auto);
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/");
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                format!("{dir}{name}.rectified.jpg"),
                encode_jpeg(&out, 85).unwrap(),
            )
            .unwrap();
            let small = imageops::resize(&dbg, 816, 612, imageops::FilterType::Triangle);
            std::fs::write(
                format!("{dir}{name}.debug.jpg"),
                encode_jpeg(&small, 85).unwrap(),
            )
            .unwrap();
        }
    }
}
