//! Several screens in one (wide-angle) frame.
//!
//! The single-screen detector judges outlines by how much of the frame's
//! bright content they hold, which only works when one screen dominates the
//! view. Here every outline has to be drawn by real edges instead: straight
//! lines are found with a Hough transform voted by edge pixels along their
//! gradient, every four of them that close a plausible quadrilateral is a
//! candidate, and a candidate counts only when each of its four sides is
//! backed by an edge along most of its length. A patch of desk between
//! screens, or an outline cutting diagonally across one, always has a side
//! running through open space and drops out.

use super::*;
use imageproc::gradients::{horizontal_sobel, vertical_sobel};

/// How far (px) an edge pixel may lie off a line and still back it.
const NEAR: f32 = 1.5;
/// How far (degrees) an edge pixel's own direction may differ from the line's.
const ALIGN: f32 = 12.0;

/// A straight line `x cos θ + y sin θ = r`, with which positions along it
/// (t = the coordinate along the line, 1 px bins from `t0`) are backed by
/// edge pixels, as a prefix sum.
struct Line {
    theta: f32,
    r: f32,
    t0: f32,
    backed: Vec<u32>,
}

impl Line {
    fn normal(&self) -> Pt {
        (self.theta.cos(), self.theta.sin())
    }

    fn along(&self, p: Pt) -> f32 {
        -p.0 * self.theta.sin() + p.1 * self.theta.cos()
    }

    /// Share of the stretch between two positions along the line that is
    /// backed by edges, leaving out a little at each end (corners round off).
    fn support(&self, a: f32, b: f32) -> f32 {
        let (a, b) = (a.min(b), a.max(b));
        let trim = (b - a) * 0.06;
        let n = self.backed.len() as isize - 1;
        let i = |t: f32| ((t - self.t0).round() as isize).clamp(0, n) as usize;
        let (ia, ib) = (i(a + trim), i(b - trim));
        if ib <= ia {
            return 0.0;
        }
        (self.backed[ib] - self.backed[ia]) as f32 / (ib - ia) as f32
    }

    fn intersect(&self, o: &Line) -> Option<Pt> {
        let (a1, b1) = self.normal();
        let (a2, b2) = o.normal();
        let det = a1 * b2 - a2 * b1;
        if det.abs() < 1e-3 {
            return None;
        }
        Some((
            (self.r * b2 - o.r * b1) / det,
            (a1 * o.r - a2 * self.r) / det,
        ))
    }
}

/// An edge pixel and its direction (the gradient's, as an angle in [0, π)).
struct Edgel {
    p: Pt,
    dir: f32,
}

fn edgels(rgb: &RgbImage) -> (Vec<Edgel>, u32, u32) {
    let (w, h) = rgb.dimensions();
    let gray = luma_of(rgb);
    let smooth = gaussian_blur_f32(&gray, 1.0);
    // Low thresholds: a dark panel in a black bezel is a small step, and
    // clutter is weeded out later by asking for long straight runs.
    let edges = canny(&smooth, 10.0, 28.0);
    let gx = horizontal_sobel(&smooth);
    let gy = vertical_sobel(&smooth);
    // The black surround a lens correction leaves is not a screen's edge.
    let void = dilate(
        &threshold(&gray, 1, ThresholdType::BinaryInverted),
        Norm::LInf,
        3,
    );
    let mut out = Vec::new();
    for (x, y, e) in edges.enumerate_pixels() {
        if e[0] == 0 || void.get_pixel(x, y)[0] > 0 {
            continue;
        }
        let (dx, dy) = (gx.get_pixel(x, y)[0] as f32, gy.get_pixel(x, y)[0] as f32);
        let dir = dy.atan2(dx).rem_euclid(std::f32::consts::PI);
        out.push(Edgel {
            p: (x as f32, y as f32),
            dir,
        });
    }
    (out, w, h)
}

fn angle_gap(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(std::f32::consts::PI);
    d.min(std::f32::consts::PI - d)
}

/// The longest straight lines. Edge pixels are grouped into chains of
/// nearly the same direction (a curve drifts out of its chain within a few
/// pixels, so glare spots and round objects only give short ones), each
/// chain long and thin enough is fitted with a line, and chains on the same
/// line merge. A line is then backed by every aligned edge pixel on it.
fn lines(edgels: &[Edgel], w: u32, h: u32, min_len: f32, keep: usize) -> Vec<Line> {
    let mut at = vec![u32::MAX; (w * h) as usize];
    for (i, e) in edgels.iter().enumerate() {
        at[(e.p.1 as u32 * w + e.p.0 as u32) as usize] = i as u32;
    }
    let mut taken = vec![false; edgels.len()];
    // (θ, r, length) of every straight chain.
    let mut chains: Vec<(f32, f32, f32)> = Vec::new();
    let mut region: Vec<usize> = Vec::new();
    for seed in 0..edgels.len() {
        if taken[seed] {
            continue;
        }
        taken[seed] = true;
        region.clear();
        region.push(seed);
        // Mean direction as a sum of doubled angles (directions wrap at π).
        let (mut sc, mut ss) = (
            (2.0 * edgels[seed].dir).cos(),
            (2.0 * edgels[seed].dir).sin(),
        );
        let mut k = 0;
        while k < region.len() {
            let e = &edgels[region[k]];
            k += 1;
            let mean = 0.5 * ss.atan2(sc);
            let (x, y) = (e.p.0 as i32, e.p.1 as i32);
            for (dx, dy) in [
                (-1, -1),
                (0, -1),
                (1, -1),
                (-1, 0),
                (1, 0),
                (-1, 1),
                (0, 1),
                (1, 1),
            ] {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let n = at[(ny as u32 * w + nx as u32) as usize];
                if n == u32::MAX || taken[n as usize] {
                    continue;
                }
                let d = edgels[n as usize].dir;
                if angle_gap(d, mean) <= 20f32.to_radians() {
                    taken[n as usize] = true;
                    region.push(n as usize);
                    sc += (2.0 * d).cos();
                    ss += (2.0 * d).sin();
                }
            }
        }
        if (region.len() as f32) < 0.7 * min_len {
            continue;
        }
        // Principal axis of the chain.
        let n = region.len() as f32;
        let (mx, my) = region.iter().fold((0.0, 0.0), |a, &i| {
            (a.0 + edgels[i].p.0, a.1 + edgels[i].p.1)
        });
        let (mx, my) = (mx / n, my / n);
        let (mut sxx, mut sxy, mut syy) = (0.0f32, 0.0f32, 0.0f32);
        for &i in &region {
            let (dx, dy) = (edgels[i].p.0 - mx, edgels[i].p.1 - my);
            sxx += dx * dx;
            sxy += dx * dy;
            syy += dy * dy;
        }
        let along = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let theta = (along + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::PI);
        let (c, s) = (theta.cos(), theta.sin());
        let r = mx * c + my * s;
        let (mut lo, mut hi, mut off2) = (f32::MAX, f32::MIN, 0.0f32);
        for &i in &region {
            let p = edgels[i].p;
            let t = -p.0 * s + p.1 * c;
            lo = lo.min(t);
            hi = hi.max(t);
            off2 += (p.0 * c + p.1 * s - r).powi(2);
        }
        if hi - lo >= min_len && (off2 / n).sqrt() <= 1.2 {
            chains.push((theta, r, hi - lo));
        }
    }
    chains.sort_by(|a, b| b.2.total_cmp(&a.2));
    let diag = ((w * w + h * h) as f32).sqrt();
    let nr = (2.0 * diag) as usize + 1;
    let mut out: Vec<(u32, Line)> = Vec::new();
    for (theta, r, _) in chains {
        let (c, s) = (theta.cos(), theta.sin());
        // Already part of a longer chain's line?
        if out.iter().any(|(_, l)| {
            angle_gap(l.theta, theta) <= 3f32.to_radians() && {
                let (lc, ls) = l.normal();
                // The chain's foot point lies on that line.
                ((r * c) * lc + (r * s) * ls - l.r).abs() <= 2.5
            }
        }) {
            continue;
        }
        let mut line = Line {
            theta,
            r,
            t0: -diag,
            backed: Vec::new(),
        };
        let mut hit = vec![false; nr];
        for e in edgels {
            if (e.p.0 * c + e.p.1 * s - r).abs() <= NEAR
                && angle_gap(e.dir, theta) <= ALIGN.to_radians()
            {
                let t = line.along(e.p);
                hit[((t + diag).round() as usize).min(nr - 1)] = true;
            }
        }
        // Bridge one-pixel gaps (a near-diagonal line steps by √2).
        let mut prefix = Vec::with_capacity(nr + 1);
        prefix.push(0);
        for i in 0..nr {
            let b = hit[i] || (i > 0 && i + 1 < nr && hit[i - 1] && hit[i + 1]);
            prefix.push(prefix[i] + b as u32);
        }
        line.backed = prefix;
        out.push((longest_run(&hit), line));
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.0));
    out.truncate(keep);
    out.into_iter().map(|(_, l)| l).collect()
}

/// The longest stretch of backed positions, bridging gaps of up to 3 px.
fn longest_run(hit: &[bool]) -> u32 {
    let (mut best, mut start, mut last) = (0usize, None::<usize>, 0usize);
    for (i, &h) in hit.iter().enumerate() {
        if !h {
            continue;
        }
        match start {
            Some(s) if i - last <= 4 => best = best.max(i - s + 1),
            _ => start = Some(i),
        }
        last = i;
    }
    best as u32
}

/// A screen outline found from lines, with how well each side is backed.
#[derive(Debug, Clone, Copy)]
pub(super) struct Found {
    pub q: Quad,
    pub score: f32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub sides: [f32; 4],
}

/// Every outline closed by four well-backed lines, best first, overlapping
/// ones resolved (the better kept) and outlines around several others dropped.
pub(super) fn outlines(rgb: &RgbImage, min_area_ratio: f32) -> Vec<Found> {
    let (edgels, w, h) = edgels(rgb);
    separate(outlines_of(rgb, &edgels, w, h, min_area_ratio))
}

/// Every outline closed by four lines with three or four of its sides
/// backed, best first, overlapping ones included.
pub(super) fn all_outlines(rgb: &RgbImage, min_area_ratio: f32) -> Vec<Found> {
    let (edgels, w, h) = edgels(rgb);
    outlines_of(rgb, &edgels, w, h, min_area_ratio)
}

/// How much of each side of `q` (top, right, bottom, left) runs along an
/// edge, and the outlines `outlines` finds, from one pass over the edges.
pub(super) fn sides_and_outlines(
    rgb: &RgbImage,
    q: &Quad,
    min_area_ratio: f32,
) -> ([f32; 4], Vec<Found>) {
    let (edgels, w, h) = edgels(rgb);
    let mut dir = vec![f32::NAN; (w * h) as usize];
    for e in &edgels {
        dir[(e.p.1 as u32 * w + e.p.0 as u32) as usize] = e.dir;
    }
    let mut sides = [0.0f32; 4];
    for (k, side) in sides.iter_mut().enumerate() {
        let (a, b) = (q.0[k], q.0[(k + 1) % 4]);
        let len = dist(a, b);
        let (ux, uy) = ((b.0 - a.0) / len.max(1.0), (b.1 - a.1) / len.max(1.0));
        let normal = (ux.atan2(-uy)).rem_euclid(std::f32::consts::PI);
        let n = len.round().max(1.0) as usize;
        let mut hits = 0;
        for i in 0..n {
            let t = 0.06 + 0.88 * (i as f32 + 0.5) / n as f32;
            let (x, y) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            let hit = (-2..=2).any(|o| {
                let o = o as f32 * 0.75;
                let (px, py) = ((x - uy * o).round(), (y + ux * o).round());
                px >= 0.0
                    && py >= 0.0
                    && (px as u32) < w
                    && (py as u32) < h
                    && angle_gap(dir[(py as u32 * w + px as u32) as usize], normal)
                        <= ALIGN.to_radians()
            });
            hits += hit as u32;
        }
        *side = hits as f32 / n as f32;
    }
    (
        sides,
        separate(outlines_of(rgb, &edgels, w, h, min_area_ratio)),
    )
}

fn outlines_of(
    #[cfg_attr(not(test), allow(unused_variables))] rgb: &RgbImage,
    edgels: &[Edgel],
    w: u32,
    h: u32,
    min_area_ratio: f32,
) -> Vec<Found> {
    let short = w.min(h) as f32;
    let lines = lines(edgels, w, h, 0.07 * short, 70);
    #[cfg(test)]
    for l in &lines {
        let run = l.backed.last().copied().unwrap_or(0);
        eprintln!(
            "  line θ={:.1} r={:.1} backed={run}",
            l.theta.to_degrees(),
            l.r
        );
    }
    #[cfg(test)]
    {
        let mut img = rgb.clone();
        for e in edgels {
            img.put_pixel(e.p.0 as u32, e.p.1 as u32, Rgb([0, 255, 255]));
        }
        for l in &lines {
            let (c, s) = l.normal();
            let n = l.backed.len();
            for i in 1..n {
                let t = l.t0 + i as f32 - 1.0;
                let (x, y) = (l.r * c - t * s, l.r * s + t * c);
                if x >= 0.0 && y >= 0.0 && (x as u32) < w && (y as u32) < h {
                    let b = l.backed[i] > l.backed[i - 1];
                    img.put_pixel(
                        x as u32,
                        y as u32,
                        if b {
                            Rgb([255, 0, 0])
                        } else {
                            Rgb([255, 255, 0])
                        },
                    );
                }
            }
        }
        DEBUG_DIR.with(|d| {
            if let Some(dir) = d.borrow().as_ref() {
                let _ = img.save(format!("{dir}/lines.png"));
            }
        });
    }
    let frame_area = (w * h) as f32;
    // A screen may run off the edge of the view (a corner cut off by the
    // frame or by the black surround of a lens correction).
    let slack = 0.25 * short;
    let inside = |p: Pt| {
        (-slack..=w as f32 + slack).contains(&p.0) && (-slack..=h as f32 + slack).contains(&p.1)
    };

    // Opposite sides: nearly parallel (perspective tilts them a little), apart.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..lines.len() {
        for j in i + 1..lines.len() {
            if angle_gap(lines[i].theta, lines[j].theta) > 20f32.to_radians() {
                continue;
            }
            let (ci, cj) = (lines[i].normal(), lines[j].normal());
            let same = ci.0 * cj.0 + ci.1 * cj.1 > 0.0;
            let gap = if same {
                (lines[i].r - lines[j].r).abs()
            } else {
                (lines[i].r + lines[j].r).abs()
            };
            if gap >= 0.1 * short {
                pairs.push((i, j));
            }
        }
    }

    let mut found: Vec<Found> = Vec::new();
    for (pi, &(a, b)) in pairs.iter().enumerate() {
        for &(c, d) in &pairs[pi + 1..] {
            let cross = angle_gap(lines[a].theta, lines[c].theta);
            if cross < 55f32.to_radians() {
                continue;
            }
            // Corners in order around the outline: a∩c, c∩b, b∩d, d∩a.
            let order = [(a, c), (c, b), (b, d), (d, a)];
            let mut pts = [(0.0f32, 0.0f32); 4];
            let mut ok = true;
            for (k, &(l1, l2)) in order.iter().enumerate() {
                match lines[l1].intersect(&lines[l2]) {
                    Some(p) if inside(p) => pts[k] = p,
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || !is_convex(&pts) {
                continue;
            }
            let area = polygon_area(&pts);
            if area < min_area_ratio * frame_area || area > 0.8 * frame_area {
                continue;
            }
            // Side k runs from corner k to corner k+1 on line order[k].1 / order[k+1].0.
            let side_lines = [c, b, d, a];
            let mut sides = [0.0f32; 4];
            let mut shortest = f32::MAX;
            for k in 0..4 {
                let (p, q) = (pts[k], pts[(k + 1) % 4]);
                shortest = shortest.min(dist(p, q));
                let l = &lines[side_lines[k]];
                // Judged on the part in view, which must be most of it.
                sides[k] = match clip(p, q, w as f32, h as f32) {
                    Some((p2, q2)) if dist(p2, q2) >= 0.6 * dist(p, q) => {
                        l.support(l.along(p2), l.along(q2))
                    }
                    _ => 0.0,
                };
            }
            if shortest < 0.1 * short {
                continue;
            }
            // One side may be mostly hidden (a cable, a hand, glare across
            // a corner); the other three must be there.
            let mut sorted = sides;
            sorted.sort_by(f32::total_cmp);
            let (weakest, second) = (sorted[0], sorted[1]);
            if second < 0.5 || weakest < 0.15 {
                continue;
            }
            let q = Quad::ordered(pts);
            let aspect = aspect_score(&q);
            if aspect <= 0.0 {
                continue;
            }
            let mean = sides.iter().sum::<f32>() / 4.0;
            let score = mean * (0.5 + 0.5 * weakest) * (0.5 + 0.5 * aspect);
            found.push(Found { q, score, sides });
        }
    }
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    #[cfg(test)]
    for f in found.iter().take(25) {
        eprintln!(
            "  outline {:.3} sides {:?} {:?}",
            f.score,
            f.sides.map(|s| (s * 100.0) as i32),
            f.q.0.map(|p| (p.0 as i32, p.1 as i32))
        );
    }
    found
}

/// The part of the segment p→q inside the w×h image (Liang–Barsky).
fn clip(p: Pt, q: Pt, w: f32, h: f32) -> Option<(Pt, Pt)> {
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for (den, num) in [
        (-dx, p.0),
        (dx, w - 1.0 - p.0),
        (-dy, p.1),
        (dy, h - 1.0 - p.1),
    ] {
        if den == 0.0 {
            if num < 0.0 {
                return None;
            }
        } else {
            let t = num / den;
            if den < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
        }
    }
    (t0 < t1).then_some((
        (p.0 + dx * t0, p.1 + dy * t0),
        (p.0 + dx * t1, p.1 + dy * t1),
    ))
}

/// Of overlapping outlines the better stays; an outline holding two or more
/// separate good ones is a box around several screens and goes.
fn separate(found: Vec<Found>) -> Vec<Found> {
    let top = found.first().map_or(0.0, |f| f.score);
    let found: Vec<Found> = found.into_iter().filter(|f| f.score >= 0.5 * top).collect();
    let container = |f: &Found| {
        let inner: Vec<&Found> = found
            .iter()
            .filter(|o| {
                o.score >= 0.7 * f.score
                    && o.q.area() <= 0.7 * f.q.area()
                    && share_inside(&o.q, &f.q) >= 0.8
            })
            .collect();
        inner.iter().enumerate().any(|(i, a)| {
            inner[i + 1..]
                .iter()
                .any(|b| share_inside(&a.q, &b.q) < 0.2 && share_inside(&b.q, &a.q) < 0.2)
        })
    };
    let mut kept: Vec<Found> = Vec::new();
    for f in &found {
        if container(f) {
            continue;
        }
        if kept
            .iter()
            .all(|k| share_inside(&f.q, &k.q) < 0.3 && share_inside(&k.q, &f.q) < 0.3)
        {
            kept.push(*f);
        }
    }
    #[cfg(test)]
    for k in &kept {
        eprintln!(
            "  kept {:.3} sides {:?} {:?}",
            k.score,
            k.sides.map(|s| (s * 100.0) as i32),
            k.q.0.map(|p| (p.0 as i32, p.1 as i32))
        );
    }
    kept
}
