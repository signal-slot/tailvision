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
use imageproc::geometric_transformations::{
    Border, Interpolation, Projection, warp_into, warp_into_with,
};
use imageproc::geometry::{approximate_polygon_dp, arc_length, convex_hull, min_area_rect};
use imageproc::morphology::{close, dilate, erode};
use imageproc::point::Point;
use imageproc::region_labelling::{Connectivity, connected_components};

mod multi;

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

/// Median grey level under the mask: the background of a UI, unmoved by its
/// bright text and panels, where the mean would be.
fn median_under(gray: &GrayImage, mask: &GrayImage) -> Option<f32> {
    let mut hist = [0u32; 256];
    let mut n = 0u32;
    for (g, m) in gray.pixels().zip(mask.pixels()) {
        if m[0] > 0 {
            hist[g[0] as usize] += 1;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let mut seen = 0u32;
    for (v, c) in hist.iter().enumerate() {
        seen += c;
        if seen * 2 >= n {
            return Some(v as f32);
        }
    }
    Some(255.0)
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

/// (contrast, outside_uniformity, inner_fit): how real the quad's boundary
/// is, as the brightness step between a thin ring just inside and just
/// outside it (saturating at ~20 grey levels); how uniform the outside ring
/// is; and how close the inside ring's median is to the median of the quad's
/// interior (medians, so a dark UI's bright text does not count as its
/// background). An LCD's edge is bordered by its bezel, which is plain; the
/// bezel's own outer edge is bordered by the board or desk, which is
/// cluttered. And the ring just inside an LCD's edge is panel like the rest
/// of the quad, while the ring just inside the bezel's outer edge is bezel
/// around a panel of another brightness. Together they tell the two apart
/// when both have a crisp step, even when a sticker or a logo clutters the
/// bezel.
fn border_contrast(gray: &GrayImage, q: &Quad, band: u8) -> (f32, f32, f32) {
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
        return (0.0, 0.0, 0.0);
    };
    let contrast = ((a - b).abs() / 255.0 / 0.08).min(1.0);
    let inner_fit = match (median_under(gray, &inside), median_under(gray, &inner_b)) {
        (Some(ring), Some(interior)) => {
            (1.0 - (ring - interior).abs() / 255.0 / 0.25).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let (mut sum2, mut n) = (0.0f64, 0u32);
    for (g, m) in gray.pixels().zip(outside.pixels()) {
        if m[0] > 0 {
            sum2 += (g[0] as f64 - b as f64).powi(2);
            n += 1;
        }
    }
    let std = (sum2 / n.max(1) as f64).sqrt() as f32;
    (contrast, (1.0 - std / 40.0).clamp(0.0, 1.0), inner_fit)
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

/// A candidate outline with everything the scores are made of.
#[derive(Debug, Clone)]
struct Candidate {
    origin: &'static str,
    q: Quad,
    /// A real brightness step along the outline (0 = none).
    boundary: f32,
    outside_plain: f32,
    inner_fit: f32,
    aspect: f32,
    area_ratio: f32,
    /// Share of all the frame's bright content inside the outline.
    coverage: f32,
    tight: f32,
}

/// Every candidate outline in a (small) frame, measured, with frame-wide
/// quads and duplicates dropped.
struct Candidates {
    list: Vec<Candidate>,
    /// Enough bright content in the frame for the coverage terms to mean anything.
    use_coverage: bool,
    /// The bright content's pixels.
    content: Vec<Pt>,
}

fn screen_candidates(rgb: &RgbImage, min_area_ratio: f32) -> Candidates {
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
    let mut out = Vec::new();
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
        let (mut boundary, outside_plain, mut inner_fit) = border_contrast(&gray, &q, band);
        if origin == "ui" {
            // Bounded by the content rather than by a bezel step.
            boundary = boundary.max(0.5);
            inner_fit = 1.0;
        }
        out.push(Candidate {
            origin,
            q,
            boundary,
            outside_plain,
            inner_fit,
            aspect: aspect_score(&q),
            area_ratio: (q.area() / frame_area).min(0.90),
            coverage: content_coverage(&content, &q, total_bright),
            tight: tightness(&q),
        });
    }
    Candidates {
        list: out,
        use_coverage,
        content: content
            .enumerate_pixels()
            .filter(|(_, _, p)| p[0] > 0)
            .map(|(x, y, _)| (x as f32, y as f32))
            .collect(),
    }
}

/// The score of a candidate as the screen, from its coverage and tightness
/// (the frame-wide ones, or those of `local_terms`).
fn score(c: &Candidate, use_coverage: bool, coverage: f32, tight: f32) -> f32 {
    if use_coverage {
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
            * c.boundary
            * (0.25 * c.aspect
                + 0.05 * c.area_ratio
                + 0.25 * coverage
                + 0.10 * c.outside_plain
                + 0.15 * tight
                + 0.20 * c.inner_fit)
    } else {
        c.boundary
            * (0.35 * c.aspect + 0.30 * c.area_ratio + 0.15 * c.outside_plain + 0.20 * c.inner_fit)
    }
}

/// The LCD-likeliest convex quad in a (small) frame, or None. With several
/// screens in view the frame-wide scores favour an outline around all of
/// them; when the pick holds two or more screens judged one by one with
/// dark between them (desk and bezels, where tiles of one UI sit on its lit
/// background), the best of those is taken instead.
pub fn find_screen_quad(rgb: &RgbImage, min_area_ratio: f32) -> Option<Quad> {
    let cands = screen_candidates(rgb, min_area_ratio);
    let single = best_single(&cands)?.q;
    // best_each never keeps two overlapping outlines, so these are separate.
    let held: Vec<(f32, Quad)> = best_each(&cands)
        .into_iter()
        .filter(|(_, q)| share_inside(q, &single) >= 0.8 && q.area() <= 0.7 * single.area())
        .collect();
    if held.len() >= 2 && dark_between(rgb, &single, &held) {
        #[cfg(test)]
        eprintln!(
            "  single pick holds {} screens with dark between them; taking the best of them",
            held.len()
        );
        return Some(held[0].1);
    }
    // Screens drawn by their edges, and the pick (with no edges of its own)
    // is none of them, or cuts through one: it spans some of them, or is a
    // patch of desk.
    // A real screen's outline runs along edges on (nearly) all four sides.
    let (sides, outlines) = multi::sides_and_outlines(rgb, &single, min_area_ratio);
    let mut sorted = sides;
    sorted.sort_by(f32::total_cmp);
    #[cfg(test)]
    eprintln!(
        "  single pick sides {:?}",
        sides.map(|s| (s * 100.0) as i32)
    );
    let same =
        |o: &multi::Found| share_inside(&o.q, &single) >= 0.8 && share_inside(&single, &o.q) >= 0.8;
    // An outline the pick cuts through: part of it inside, part outside.
    let crossed = |o: &multi::Found| (0.15..0.8).contains(&share_inside(&o.q, &single));
    if sorted[1] < 0.5
        && ((outlines.len() >= 2 && !outlines.iter().any(same)) || outlines.iter().any(crossed))
    {
        #[cfg(test)]
        eprintln!(
            "  single pick is none of {} outlined screens; taking the best",
            outlines.len()
        );
        return Some(outlines[0].q);
    }
    Some(single)
}

/// True when the part of `outer` outside the `inner` outlines is dark next
/// to them: under a quarter of their mean brightness.
fn dark_between(rgb: &RgbImage, outer: &Quad, inner: &[(f32, Quad)]) -> bool {
    let gray = luma_of(rgb);
    let (w, h) = gray.dimensions();
    let mut gap = filled_mask(w, h, outer);
    let mut held = GrayImage::new(w, h);
    for (_, q) in inner {
        // Grown a little so their own bezels do not count as the gap.
        draw_polygon_mut(&mut gap, &q.with_margin(-0.1).to_i32(), Luma([0u8]));
        draw_polygon_mut(&mut held, &q.to_i32(), Luma([255u8]));
    }
    match (mean_under(&gray, &gap), mean_under(&gray, &held)) {
        (Some(g), Some(s)) => g < 0.25 * s,
        _ => false,
    }
}

/// The pick by frame-wide scores, made for a frame with one screen in it.
fn best_single(cands: &Candidates) -> Option<&Candidate> {
    let use_coverage = cands.use_coverage;
    let mut best_real: Option<(f32, &Candidate)> = None;
    let mut best_any: Option<(f32, &Candidate)> = None;
    for c in &cands.list {
        let score = score(c, use_coverage, c.coverage, c.tight);
        #[cfg(test)]
        eprintln!(
            "  {:6} score={score:.3} boundary={:.2} plain={:.2} fit={:.2} tight={:.2} aspect={:.2} area={:.2} cov={:.2} {:?}",
            c.origin,
            c.boundary,
            c.outside_plain,
            c.inner_fit,
            c.tight,
            c.aspect,
            c.area_ratio,
            c.coverage,
            c.q.0.map(|p| (p.0 as i32, p.1 as i32))
        );
        if c.boundary <= 0.0 || score <= 0.0 {
            continue;
        }
        if c.origin != "ui"
            && (!use_coverage || c.coverage >= 0.9)
            && best_real.is_none_or(|(s, _)| score > s)
        {
            best_real = Some((score, c));
        }
        if best_any.is_none_or(|(s, _)| score > s) {
            best_any = Some((score, c));
        }
    }
    best_real.or(best_any).map(|(_, c)| c)
}

/// True when `p` lies inside the convex quad.
fn inside_quad(q: &Quad, p: Pt) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (q.0[i], q.0[(i + 1) % 4]);
        let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        if cross != 0.0 {
            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }
    }
    true
}

/// Roughly how much of `a` lies inside `b`, from a grid of points over `a`.
fn share_inside(a: &Quad, b: &Quad) -> f32 {
    const N: usize = 12;
    let [tl, tr, br, bl] = a.0;
    let lerp = |p: Pt, q: Pt, t: f32| (p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t);
    let mut hits = 0;
    for i in 0..N {
        let v = (i as f32 + 0.5) / N as f32;
        let (l, r) = (lerp(tl, bl, v), lerp(tr, br, v));
        for j in 0..N {
            if inside_quad(b, lerp(l, r, (j as f32 + 0.5) / N as f32)) {
                hits += 1;
            }
        }
    }
    hits as f32 / (N * N) as f32
}

/// Coverage and tightness judged against the content near the outline only:
/// the share of the content within a ring a fifth of the outline's size wide
/// that lies inside it, and how much of its bounding box that content spans.
/// A whole screen scores high however many other screens are in view; a card
/// on a screen leaves the rest of that screen's content in the ring.
fn local_terms(q: &Quad, content: &[Pt]) -> (f32, f32) {
    let around = q.with_margin(-0.4);
    let xs = around.0.iter().map(|p| p.0);
    let ys = around.0.iter().map(|p| p.1);
    let (ax0, ax1) = (
        xs.clone().fold(f32::MAX, f32::min),
        xs.fold(f32::MIN, f32::max),
    );
    let (ay0, ay1) = (
        ys.clone().fold(f32::MAX, f32::min),
        ys.fold(f32::MIN, f32::max),
    );
    let (mut n_in, mut n_around) = (0u32, 0u32);
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &p in content {
        if p.0 < ax0 || p.0 > ax1 || p.1 < ay0 || p.1 > ay1 || !inside_quad(&around, p) {
            continue;
        }
        n_around += 1;
        if inside_quad(q, p) {
            n_in += 1;
            x0 = x0.min(p.0);
            y0 = y0.min(p.1);
            x1 = x1.max(p.0);
            y1 = y1.max(p.1);
        }
    }
    if n_in == 0 {
        return (0.0, 0.0);
    }
    let xs = q.0.iter().map(|p| p.0);
    let ys = q.0.iter().map(|p| p.1);
    let bw = xs.clone().fold(f32::MIN, f32::max) - xs.fold(f32::MAX, f32::min);
    let bh = ys.clone().fold(f32::MIN, f32::max) - ys.fold(f32::MAX, f32::min);
    let tight = ((x1 - x0 + 1.0) * (y1 - y0 + 1.0) / (bw * bh).max(1.0)).min(1.0);
    (n_in as f32 / n_around as f32, tight)
}

/// Every screen in a (small) frame, best first: each candidate is scored
/// against the content around it instead of the whole frame's, outlines that
/// span two or more separate screens are dropped, and of overlapping
/// outlines the better one stays.
pub fn find_screen_quads(rgb: &RgbImage, min_area_ratio: f32) -> Vec<(f32, Quad)> {
    best_each(&screen_candidates(rgb, min_area_ratio))
}

fn best_each(cands: &Candidates) -> Vec<(f32, Quad)> {
    let mut scored: Vec<(f32, Quad)> = cands
        .list
        .iter()
        // The content's bounding box spans every screen at once.
        .filter(|c| c.origin != "ui" && c.boundary > 0.0)
        .filter_map(|c| {
            let (coverage, tight) = local_terms(&c.q, &cands.content);
            let s = score(c, cands.use_coverage, coverage, tight);
            #[cfg(test)]
            eprintln!(
                "  multi {:6} score={s:.3} local_cov={coverage:.2} local_tight={tight:.2} {:?}",
                c.origin,
                c.q.0.map(|p| (p.0 as i32, p.1 as i32))
            );
            (s > 0.0).then_some((s, c.q))
        })
        .collect();
    let Some(top) = scored.iter().map(|(s, _)| *s).reduce(f32::max) else {
        return Vec::new();
    };
    scored.retain(|(s, _)| *s >= 0.35 * top);
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    // An outline holding two good outlines that do not overlap each other
    // is a box around several screens.
    let containers: Vec<bool> = scored
        .iter()
        .map(|(s, q)| {
            let inner: Vec<&Quad> = scored
                .iter()
                .filter(|(si, qi)| {
                    *si >= 0.6 * s && qi.area() <= 0.7 * q.area() && share_inside(qi, q) >= 0.8
                })
                .map(|(_, qi)| qi)
                .collect();
            inner.iter().enumerate().any(|(i, a)| {
                inner[i + 1..]
                    .iter()
                    .any(|b| share_inside(a, b) < 0.2 && share_inside(b, a) < 0.2)
            })
        })
        .collect();
    let mut kept: Vec<(f32, Quad)> = Vec::new();
    for ((s, q), container) in scored.into_iter().zip(containers) {
        if container {
            continue;
        }
        if kept
            .iter()
            .all(|(_, k)| share_inside(&q, k) < 0.5 && share_inside(k, &q) < 0.5)
        {
            kept.push((s, q));
        }
    }
    kept
}

/// Every screen, best first. Outlines drawn by straight edges on (at least
/// three of) their four sides come first (see `multi`). In a dark room a
/// screen with a black UI has no visible edge at all, only its lit content,
/// so screens found by their content are added where no outline is, when
/// they glow out of darkness.
pub fn find_all_screens(rgb: &RgbImage, min_area_ratio: f32) -> Vec<(f32, Quad)> {
    let mut all: Vec<(f32, Quad)> = multi::outlines(rgb, min_area_ratio)
        .into_iter()
        .map(|f| (f.score, f.q))
        .collect();
    let gray = luma_of(rgb);
    // Only a dark room can hide a screen's edges; elsewhere the search by
    // content costs seconds on a Zero and finds desk.
    if median(&gray) >= 15 {
        return all;
    }
    for (s, q) in glowing_screens(rgb, min_area_ratio) {
        let free = all
            .iter()
            .all(|(_, o)| share_inside(&q, o) < 0.3 && share_inside(o, &q) < 0.3);
        if free && in_the_dark(&gray, &q) {
            all.push((s, q));
        }
    }
    all
}

/// True when the surroundings of `q` are dark, and much darker than it.
fn in_the_dark(gray: &GrayImage, q: &Quad) -> bool {
    let (w, h) = gray.dimensions();
    let mut ring = filled_mask(w, h, &q.with_margin(-0.15));
    draw_polygon_mut(&mut ring, &q.with_margin(-0.03).to_i32(), Luma([0u8]));
    let inside = filled_mask(w, h, q);
    match (mean_under(gray, &ring), mean_under(gray, &inside)) {
        (Some(r), Some(i)) => {
            #[cfg(test)]
            eprintln!(
                "  glow {:?} ring={r:.1} inside={i:.1}",
                q.0.map(|p| (p.0 as i32, p.1 as i32))
            );
            r < 12.0 && r < 0.4 * i
        }
        _ => false,
    }
}

/// Below this an outline is a dark patch of desk, not a screen.
const MIN_SCREEN_SCORE: f32 = 0.15;

/// Screens found by their bright content, peeling: after each round the
/// screens found are painted over with the frame's dark level and the rest
/// is searched again, so a dim screen gets thresholds of its own instead of
/// the brightest screen's; each is then detected again in a close-up.
fn glowing_screens(rgb: &RgbImage, min_area_ratio: f32) -> Vec<(f32, Quad)> {
    let dark = {
        let v = percentile(&value_of(rgb), 10.0);
        Rgb([v, v, v])
    };
    let mut img = rgb.clone();
    let mut found: Vec<(f32, Quad)> = Vec::new();
    for _ in 0..4 {
        let round = find_screen_quads(&img, min_area_ratio);
        // Only the confident ones: a weak outline is often a piece of a dim
        // screen that the next round, with that screen's own thresholds,
        // finds whole.
        let top = round.first().map_or(0.0, |(s, _)| *s);
        let new: Vec<(f32, Quad)> = round
            .into_iter()
            .filter(|(s, _)| *s >= 0.75 * top && *s >= MIN_SCREEN_SCORE)
            .filter(|(_, q)| {
                found
                    .iter()
                    .all(|(_, f)| share_inside(q, f) < 0.5 && share_inside(f, q) < 0.5)
            })
            .collect();
        if new.is_empty() {
            break;
        }
        for (_, q) in &new {
            draw_polygon_mut(&mut img, &q.with_margin(-0.06).to_i32(), dark);
        }
        found.extend(new);
    }
    (0..found.len())
        .map(|i| {
            let others: Vec<Quad> = found
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, (_, q))| *q)
                .collect();
            let (s, q) = found[i];
            (s, refine_alone(rgb, &q, &others, dark).unwrap_or(q))
        })
        .collect()
}

/// Detects the screen again in a close-up around `q`, with the other
/// screens painted out: there it dominates the view, which is what the
/// single-screen detector is tuned for, so dim screens get their real
/// corners rather than those of their brightest part. None when the
/// close-up finds something else.
fn refine_alone(rgb: &RgbImage, q: &Quad, others: &[Quad], dark: Rgb<u8>) -> Option<Quad> {
    let (crop, x0, y0, up) = close_up(rgb, q, 0.3);
    let mut crop = crop;
    let shift = |o: &Quad| Quad(o.0.map(|p| ((p.0 - x0) * up, (p.1 - y0) * up)));
    for o in others {
        draw_polygon_mut(&mut crop, &shift(o).with_margin(-0.06).to_i32(), dark);
    }
    let r = find_screen_quad(&crop, 0.1)?.scaled(1.0 / up);
    let r = Quad(r.0.map(|p| (p.0 + x0, p.1 + y0)));
    // The same screen: they mostly overlap both ways.
    (share_inside(&r, q) >= 0.6 && share_inside(q, &r) >= 0.6).then_some(r)
}

/// The bounding box of `q` grown by `margin` of its size on each side,
/// clipped to the image and scaled so its longer side is DETECT_SIZE; with
/// its top-left corner in `rgb` and the scale.
fn close_up(rgb: &RgbImage, q: &Quad, margin: f32) -> (RgbImage, f32, f32, f32) {
    let (w, h) = (rgb.width() as f32, rgb.height() as f32);
    let xs = q.0.iter().map(|p| p.0);
    let ys = q.0.iter().map(|p| p.1);
    let (x0, x1) = (
        xs.clone().fold(f32::MAX, f32::min),
        xs.fold(f32::MIN, f32::max),
    );
    let (y0, y1) = (
        ys.clone().fold(f32::MAX, f32::min),
        ys.fold(f32::MIN, f32::max),
    );
    let (mx, my) = ((x1 - x0) * margin, (y1 - y0) * margin);
    let cx0 = (x0 - mx).max(0.0).floor();
    let cy0 = (y0 - my).max(0.0).floor();
    let cx1 = (x1 + mx).min(w).ceil();
    let cy1 = (y1 + my).min(h).ceil();
    let crop = imageops::crop_imm(
        rgb,
        cx0 as u32,
        cy0 as u32,
        (cx1 - cx0) as u32,
        (cy1 - cy0) as u32,
    )
    .to_image();
    let up = DETECT_SIZE as f32 / crop.width().max(crop.height()) as f32;
    let big = imageops::resize(
        &crop,
        (crop.width() as f32 * up).round().max(1.0) as u32,
        (crop.height() as f32 * up).round().max(1.0) as u32,
        imageops::FilterType::Triangle,
    );
    (big, cx0, cy0, up)
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

/// Finds the screen in a full-resolution frame; corners are in frame
/// coordinates, those of the corrected frame when a lens is given.
pub fn detect(
    frame: &RgbImage,
    min_area_ratio: f32,
    lens: Option<&crate::lens::Lens>,
) -> Result<Quad> {
    let (small, f) = detection_copy(frame, lens)?;
    let q = find_screen_quad(&small, min_area_ratio)
        .ok_or_else(|| anyhow!("no screen-like quadrilateral found; frame the LCD so it dominates the view, add light, or pass manual_corners"))?;
    Ok(q.scaled(1.0 / f))
}

/// Every screen in a full-resolution frame, numbered left to right by
/// their centres (index 0 = leftmost); corners as for `detect`.
pub fn detect_all(
    frame: &RgbImage,
    min_area_ratio: f32,
    lens: Option<&crate::lens::Lens>,
) -> Result<Vec<Quad>> {
    let (small, f) = detection_copy(frame, lens)?;
    let mut all: Vec<Quad> = find_all_screens(&small, min_area_ratio)
        .into_iter()
        .map(|(_, q)| q.scaled(1.0 / f))
        .collect();
    if all.is_empty() {
        bail!(
            "no screen-like quadrilateral found; add light, put something bright on the screens, or pass manual_corners"
        );
    }
    all.sort_by(|a, b| a.centroid().0.total_cmp(&b.centroid().0));
    Ok(all)
}

/// Finds the screen inside a rough box `[x0, y0, x1, y1]` of the frame
/// (corrected-frame pixels with a lens, as capture_debug shows them): the
/// box is cut out, lens-corrected and scaled to the detection size, so the
/// screen dominates the view the way the single-screen detector expects,
/// however many other screens are around it.
pub fn detect_in(
    frame: &RgbImage,
    region: [f32; 4],
    lens: Option<&crate::lens::Lens>,
) -> Result<Quad> {
    let (fw, fh) = (frame.width() as f32, frame.height() as f32);
    let [x0, y0, x1, y1] = region;
    let (x0, x1) = (x0.min(x1).max(0.0), x0.max(x1).min(fw));
    let (y0, y1) = (y0.min(y1).max(0.0), y0.max(y1).min(fh));
    let (rw, rh) = (x1 - x0, y1 - y0);
    if rw < 0.03 * fw || rh < 0.03 * fh {
        bail!(
            "region [{x0:.0}, {y0:.0}, {x1:.0}, {y1:.0}] is too small or outside the {}x{} frame; give [x0, y0, x1, y1] around the whole screen",
            frame.width(),
            frame.height()
        );
    }
    let f = (DETECT_SIZE as f32 / rw.max(rh)).min(1.0);
    let (ow, oh) = ((rw * f).round() as u32, (rh * f).round() as u32);
    // Sample from a copy at most twice the output's scale, so the bilinear
    // lookups do not skip over thin lines.
    let g = (2.0 * f).min(1.0);
    let src = if g < 1.0 {
        imageops::resize(
            frame,
            (fw * g).round() as u32,
            (fh * g).round() as u32,
            imageops::FilterType::Triangle,
        )
    } else {
        frame.clone()
    };
    let mut crop = RgbImage::new(ow.max(1), oh.max(1));
    // Output pixel → corrected frame pixel → camera pixel → source copy.
    let to_frame = |u: f32, v: f32| (x0 + u / f, y0 + v / f);
    match lens {
        Some(l) => warp_into_with(
            &src,
            |u, v| {
                let p = l.distort(to_frame(u, v));
                (p.0 * g, p.1 * g)
            },
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
            &mut crop,
        ),
        None => warp_into_with(
            &src,
            |u, v| {
                let p = to_frame(u, v);
                (p.0 * g, p.1 * g)
            },
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
            &mut crop,
        ),
    }
    if looks_blanked(&luma_of(&crop)) {
        bail!("the region is dark and featureless: is the screen on, and is the region around it?");
    }
    // The screen fills most of the box: of the outlines drawn by edges the
    // big, well-backed one; failing that, the single-screen detector.
    let area = (crop.width() * crop.height()) as f32;
    let outlined = multi::all_outlines(&crop, 0.2)
        .into_iter()
        .max_by(|a, b| {
            let v = |o: &multi::Found| o.score * (o.q.area() / area);
            v(a).total_cmp(&v(b))
        })
        .map(|o| o.q);
    let q = outlined.or_else(|| find_screen_quad(&crop, 0.1)).ok_or_else(|| {
        anyhow!("no screen found in the region; make it a little larger than the screen, or pass manual_corners")
    })?;
    #[cfg(test)]
    {
        let _ = crop.save(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/region-crop.png"
        ));
    }
    Ok(Quad(q.0.map(|p| to_frame(p.0, p.1))))
}

/// The small copy detection runs on, lens-corrected, and its scale.
fn detection_copy(frame: &RgbImage, lens: Option<&crate::lens::Lens>) -> Result<(RgbImage, f32)> {
    let (small, f) = small_copy(frame);
    // Correcting the small copy is cheap, and its coordinates scale with the
    // frame's because the lens mapping is proportional to the frame size.
    let small = match lens {
        Some(l) => l.scaled(f).undistort_image(&small),
        None => small,
    };
    if looks_blanked(&luma_of(&small)) {
        bail!("frame appears blanked (lens cap, no light, or no signal)");
    }
    Ok((small, f))
}

/// Perspective-corrects the quad out of the frame and applies the output
/// options. With a lens, the corners are in corrected-frame pixels.
pub fn rectify(
    frame: &RgbImage,
    corners: Quad,
    opts: &Options,
    lens: Option<&crate::lens::Lens>,
) -> RgbImage {
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
    let black = Border::Constant(Rgb([0, 0, 0]));
    match (Projection::from_control_points(corners.0, dst), lens) {
        (Some(p), None) => warp_into(frame, p, Interpolation::Bilinear, black, &mut out),
        // One pass from the output through the corrected frame to the camera's
        // pixels; the corrected frame itself is never built.
        (Some(p), Some(lens)) => {
            let inv = p.invert();
            warp_into_with(
                frame,
                |x, y| lens.distort(inv * (x, y)),
                Interpolation::Bilinear,
                black,
                &mut out,
            );
        }
        (None, _) => {}
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
pub fn annotate(frame: &RgbImage, corners: &Quad, source: Source, screens: &[Quad]) -> RgbImage {
    let mut out = frame.clone();
    // Every screen found, in yellow with its number (for take_screenshot's
    // `screen`); the one chosen goes on top in green.
    let size = (frame.width().max(frame.height()) as f32 * 0.04).max(12.0);
    for (i, q) in screens.iter().enumerate() {
        let pts: Vec<Point<f32>> = q.0.iter().map(|p| Point::new(p.0, p.1)).collect();
        for d in -1..=1 {
            let shifted: Vec<Point<f32>> = pts
                .iter()
                .map(|p| Point::new(p.x, p.y + d as f32))
                .collect();
            draw_hollow_polygon_mut(&mut out, &shifted, Rgb([255, 220, 0]));
        }
        draw_number(&mut out, i + 1, q.centroid(), size);
    }
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

/// Draws `n` in seven-segment digits `size` pixels tall, centred on `at`,
/// yellow on a black plate so it reads on any screen content.
fn draw_number(img: &mut RgbImage, n: usize, at: Pt, size: f32) {
    use imageproc::drawing::draw_filled_rect_mut;
    use imageproc::rect::Rect;
    // Segments a..g: top, top right, bottom right, bottom, bottom left, top left, middle.
    const SEGS: [u8; 10] = [
        0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
        0b1111111, 0b1101111,
    ];
    let digits: Vec<usize> = n.to_string().bytes().map(|b| (b - b'0') as usize).collect();
    let (h, w, t) = (size, size * 0.55, (size * 0.12).max(2.0));
    let gap = w * 0.4;
    let total = digits.len() as f32 * w + (digits.len() - 1) as f32 * gap;
    let (x0, y0) = (at.0 - total / 2.0, at.1 - h / 2.0);
    let pad = t * 1.5;
    let rect = |x: f32, y: f32, rw: f32, rh: f32| {
        Rect::at(x.round() as i32, y.round() as i32)
            .of_size(rw.round().max(1.0) as u32, rh.round().max(1.0) as u32)
    };
    draw_filled_rect_mut(
        img,
        rect(x0 - pad, y0 - pad, total + 2.0 * pad, h + 2.0 * pad),
        Rgb([0, 0, 0]),
    );
    for (k, d) in digits.iter().enumerate() {
        let x = x0 + k as f32 * (w + gap);
        let half = h / 2.0;
        let segs = [
            (x, y0, w, t),
            (x + w - t, y0, t, half),
            (x + w - t, y0 + half, t, half),
            (x, y0 + h - t, w, t),
            (x, y0 + half, t, half),
            (x, y0, t, half),
            (x, y0 + half - t / 2.0, w, t),
        ];
        for (bit, (sx, sy, sw, sh)) in segs.iter().enumerate() {
            if SEGS[*d] >> bit & 1 == 1 {
                draw_filled_rect_mut(img, rect(*sx, *sy, *sw, *sh), Rgb([255, 220, 0]));
            }
        }
    }
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
        let out = rectify(&img, q, &Options::default(), None);
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
            None,
        );
        assert_eq!(rotated.dimensions(), (19, 39));
        let forced = rectify(
            &img,
            q,
            &Options {
                output_width: Some(78),
                ..Default::default()
            },
            None,
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
            let q = detect(&img, 0.05, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            let score = iou(&q, &truth, img.width(), img.height());
            if score < 0.85 {
                let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/");
                std::fs::create_dir_all(dir).unwrap();
                let _ = annotate(&img, &q, Source::Auto, &[])
                    .save(format!("{dir}synthetic-{name}.png"));
            }
            assert!(
                score >= 0.85,
                "{name}: IoU {score:.2}, got {:?}",
                q.0.map(|p| (p.0 as i32, p.1 as i32))
            );
        }
    }

    #[test]
    fn detects_and_rectifies_through_a_wide_lens() {
        use imageproc::geometric_transformations::warp_with;
        let profile = crate::lens::IMX708_WIDE;
        // A panel reaching close to the frame edges, where the bending is strongest.
        let truth = Quad([(80.0, 90.0), (880.0, 90.0), (880.0, 600.0), (80.0, 600.0)]);
        let ideal = synthetic_scene(truth, [120, 125, 130], [15, 15, 15], 150);
        let lens = profile.at(ideal.width(), ideal.height(), None);
        // What the camera sees: each of its pixels shows the scene at the corrected position.
        let seen = warp_with(
            &ideal,
            |x, y| lens.undistort((x, y)),
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
        );
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/wide-lens");
        std::fs::create_dir_all(dir).unwrap();
        let _ = seen.save(format!("{dir}/scene.png"));
        DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir.to_string()));

        let q = detect(&seen, 0.05, Some(&lens)).unwrap();
        let worst =
            q.0.iter()
                .zip(truth.0.iter())
                .map(|(a, b)| dist(*a, *b))
                .fold(0.0, f32::max);
        assert!(worst < 6.0, "corner off by {worst:.1} px: {:?}", q.0);

        let opts = Options {
            output_width: Some(400),
            ..Default::default()
        };
        let want = rectify(&ideal, truth, &opts, None);
        let got = rectify(&seen, q, &opts, Some(&lens));
        let _ = got.save(format!("{dir}/rectified.png"));
        assert_eq!(got.dimensions(), want.dimensions());
        let diff = got
            .pixels()
            .zip(want.pixels())
            .map(|(a, b)| (a[0] as f32 - b[0] as f32).abs())
            .sum::<f32>()
            / (got.width() * got.height()) as f32;
        assert!(diff < 8.0, "mean difference {diff:.1}");
        // Uncorrected, the bent edges cost visibly more.
        let plain = rectify(&seen, q, &opts, None);
        let plain_diff = plain
            .pixels()
            .zip(want.pixels())
            .map(|(a, b)| (a[0] as f32 - b[0] as f32).abs())
            .sum::<f32>()
            / (plain.width() * plain.height()) as f32;
        assert!(
            plain_diff > diff * 1.5,
            "corrected {diff:.1}, plain {plain_diff:.1}"
        );
    }

    #[test]
    fn measures_the_lens_from_the_screen_edges() {
        use imageproc::geometric_transformations::warp_with;
        let truth = Quad([(70.0, 80.0), (890.0, 80.0), (890.0, 620.0), (70.0, 620.0)]);
        let ideal = synthetic_scene(truth, [120, 125, 130], [15, 15, 15], 150);
        let (w, h) = ideal.dimensions();
        let start = crate::lens::IMX708_WIDE.at(w, h, None);
        for actual in [-0.09f32, -0.0365, -0.01] {
            let real = crate::lens::IMX708_WIDE.with_lambda(actual).at(w, h, None);
            let seen = warp_with(
                &ideal,
                |x, y| real.undistort((x, y)),
                Interpolation::Bilinear,
                Border::Constant(Rgb([0, 0, 0])),
            );
            let m = crate::lens::measure(&seen, &start).unwrap();
            eprintln!("actual {actual}: {m}");
            assert!(
                (m.lambda - actual).abs() < 0.01,
                "actual {actual}, measured {m}"
            );
            assert!(m.rms_px < 1.0, "{m}");
        }
    }

    #[test]
    fn finds_every_screen_in_a_wide_frame() {
        // A wide-angle frame with three screens: a monitor showing a dim
        // console (top left), a dark dashboard (bottom left) and a 7" panel
        // with a bright UI (right).
        let frame = fixture("scene04_three_screens_wide.jpg");
        let lens = crate::lens::IMX708_WIDE.at(frame.width(), frame.height(), None);
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/scene04");
        std::fs::create_dir_all(dir).unwrap();
        DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir.to_string()));
        let all = detect_all(&frame, 0.02, Some(&lens)).unwrap();
        for q in &all {
            eprintln!("screen {:?}", q.0.map(|p| (p.0 as i32, p.1 as i32)));
        }
        // The 7" panel, by hand on the corrected frame, at 480 px wide.
        let seven = Quad([
            (199.0, 94.0),
            (340.0, 107.0),
            (320.0, 206.0),
            (186.0, 164.0),
        ])
        .scaled(frame.width() as f32 / 480.0);
        let n = all
            .iter()
            .position(|q| share_inside(q, &seven) > 0.9 && share_inside(&seven, q) > 0.9)
            .expect("the 7\" panel is one of the screens");
        // The dashboard sits left of it, so the panel is not number 1.
        assert!(
            all.len() >= 2 && n >= 1,
            "{} screens, panel at {n}",
            all.len()
        );
        // No outline spans two screens.
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(share_inside(a, b) < 0.5 && share_inside(b, a) < 0.5);
            }
        }
        // Without a number, detection takes one of the screens, not a box around several.
        let one = detect(&frame, 0.05, Some(&lens)).unwrap();
        eprintln!("single {:?}", one.0.map(|p| (p.0 as i32, p.1 as i32)));
        assert!(
            all.iter()
                .any(|q| share_inside(q, &one) > 0.8 && share_inside(&one, q) > 0.8),
            "single pick is not one of the screens"
        );
        let corrected = lens.undistort_image(&frame);
        let dbg = annotate(&corrected, &all[n], Source::Auto, &all);
        let small = imageops::resize(&dbg, 1152, 648, imageops::FilterType::Triangle);
        let _ = small.save(format!("{dir}/annotated.png"));
    }

    #[test]
    fn finds_every_screen_in_a_lit_cluttered_frame() {
        // The same three screens with the room lit: white desk, cables, a
        // lamp's glare on two of the screens. Each screen is outlined on its
        // own, and nothing else is.
        let frame = fixture("scene05_three_screens_lit.jpg");
        let lens = crate::lens::IMX708_WIDE.at(frame.width(), frame.height(), None);
        let k = frame.width() as f32 / 480.0;
        let all: Vec<Quad> = detect_all(&frame, 0.02, Some(&lens))
            .unwrap()
            .into_iter()
            .map(|q| q.scaled(1.0 / k))
            .collect();
        for q in &all {
            eprintln!("screen {:?}", q.0.map(|p| (p.0 as i32, p.1 as i32)));
        }
        assert_eq!(all.len(), 3);
        // Left to right: dashboard, console (its centre is right of the
        // dashboard's), 7" panel; by a box well inside each.
        for (name, a, b) in [
            ("dashboard", (95.0, 125.0), (190.0, 200.0)),
            ("console", (100.0, 50.0), (200.0, 95.0)),
            ("7in", (250.0, 105.0), (370.0, 170.0)),
        ] {
            let holders = all
                .iter()
                .filter(|q| {
                    [a, (b.0, a.1), b, (a.0, b.1)]
                        .iter()
                        .all(|p| inside_quad(q, *p))
                })
                .count();
            assert_eq!(holders, 1, "{name}");
        }
        // The default pick is one of them, not the desk around them.
        let one = detect(&frame, 0.05, Some(&lens)).unwrap().scaled(1.0 / k);
        assert!(
            all.iter()
                .any(|q| share_inside(q, &one) > 0.8 && share_inside(&one, q) > 0.8)
        );
    }

    #[test]
    fn finds_the_screen_in_a_rough_region() {
        // The lit three-screen scene; boxes drawn by eye around each screen
        // on the corrected frame at 480 px wide.
        let frame = fixture("scene05_three_screens_lit.jpg");
        let lens = crate::lens::IMX708_WIDE.at(frame.width(), frame.height(), None);
        let k = frame.width() as f32 / 480.0;
        for (name, region, inner) in [
            // 7" panel, its lower-left corner hidden by glare and a cable.
            (
                "7in",
                [205.0, 80.0, 415.0, 235.0],
                [(250.0, 105.0), (370.0, 170.0)],
            ),
            (
                "dashboard",
                [55.0, 95.0, 225.0, 255.0],
                [(95.0, 125.0), (190.0, 200.0)],
            ),
            (
                "console",
                [55.0, 0.0, 235.0, 125.0],
                [(90.0, 40.0), (200.0, 95.0)],
            ),
        ] {
            let q = detect_in(&frame, region.map(|v| v * k), Some(&lens)).unwrap();
            let q = q.scaled(1.0 / k);
            eprintln!("{name}: {:?}", q.0.map(|p| (p.0 as i32, p.1 as i32)));
            // It holds the middle of the screen.
            let (a, b) = (inner[0], inner[1]);
            for p in [a, (b.0, a.1), b, (a.0, b.1)] {
                assert!(inside_quad(&q, p), "{name}: misses {p:?}");
            }
            // Near the box (the console's top-right corner is out of view).
            let (mx, my) = (0.1 * (region[2] - region[0]), 0.1 * (region[3] - region[1]));
            for p in q.0 {
                assert!(
                    p.0 >= region[0] - mx
                        && p.0 <= region[2] + mx
                        && p.1 >= region[1] - my
                        && p.1 <= region[3] + my,
                    "{name}: corner {p:?} far outside the region"
                );
            }
        }
    }

    /// Not a check: `TV_SCENE=<fixture> TV_REGION=x0,y0,x1,y1` (480-px
    /// coordinates) prints what `detect_in` finds there.
    #[test]
    #[ignore]
    fn dump_region() {
        let frame = fixture(&std::env::var("TV_SCENE").unwrap());
        let lens = crate::lens::IMX708_WIDE.at(frame.width(), frame.height(), None);
        let k = frame.width() as f32 / 480.0;
        let r: Vec<f32> = std::env::var("TV_REGION")
            .unwrap()
            .split(',')
            .map(|v| v.parse::<f32>().unwrap() * k)
            .collect();
        let q = detect_in(&frame, [r[0], r[1], r[2], r[3]], Some(&lens)).unwrap();
        eprintln!(
            "REGION {:?}",
            q.scaled(1.0 / k).0.map(|p| (p.0 as i32, p.1 as i32))
        );
    }

    /// Not a check: `TV_SCENE=<fixture> cargo test dump_scene -- --ignored`
    /// writes what detection makes of a wide-lens frame to target/screen-tests.
    #[test]
    #[ignore]
    fn dump_scene() {
        let name = std::env::var("TV_SCENE").unwrap();
        let frame = fixture(&name);
        let lens = crate::lens::IMX708_WIDE.at(frame.width(), frame.height(), None);
        let dir = format!(
            concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/{}"),
            name
        );
        std::fs::create_dir_all(&dir).unwrap();
        DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
        let all = detect_all(&frame, 0.02, Some(&lens)).unwrap_or_default();
        let one = detect(&frame, 0.05, Some(&lens)).unwrap();
        let corrected = lens.undistort_image(&frame);
        let dbg = annotate(&corrected, &one, Source::Auto, &all);
        let small = imageops::resize(&dbg, 1200, 675, imageops::FilterType::Triangle);
        small.save(format!("{dir}/annotated.png")).unwrap();
        let small = imageops::resize(&corrected, 480, 270, imageops::FilterType::Triangle);
        small.save(format!("{dir}/corrected480.png")).unwrap();
    }

    #[test]
    fn detects_the_lcd_in_the_fixture_photos() {
        for name in [
            "scene01_dark_ui_slint.png",
            "scene02_dashboard_dark.png",
            "scene03_bright_tablet_bezel.png",
        ] {
            let frame = fixture(name);
            let dir = format!(
                concat!(env!("CARGO_MANIFEST_DIR"), "/target/screen-tests/{}"),
                name
            );
            std::fs::create_dir_all(&dir).unwrap();
            DEBUG_DIR.with(|d| *d.borrow_mut() = Some(dir));
            eprintln!("== {name}");
            let q = detect(&frame, 0.05, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            eprintln!("chosen {name}: {:?}", q.0.map(|p| (p.0 as i32, p.1 as i32)));
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
                None,
            );
            let dbg = annotate(&frame, &q, Source::Auto, &[]);
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
