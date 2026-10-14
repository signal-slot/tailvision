//! Barrel distortion of wide-angle camera modules, undone so that the rest of
//! the pipeline sees straight screen edges.
//!
//! One-parameter division model around the sensor centre: a point at radius
//! ρd from the optical centre of the camera image (in units of the focal
//! length) belongs at ρu = ρd / (1 + λ ρd²) in a distortion-free image; λ < 0
//! is barrel. The corrected frame has the camera frame's size and keeps the
//! midpoints of its left and right edges, so it shows the same width of the
//! scene; the corners, which barrel distortion squeezes in, are cropped.
//!
//! λ starts as an estimate from the datasheet and is measured on the rig by
//! `measure`: the LCD's edges are straight, so the λ that straightens them is
//! the lens's (the plumb-line method). No dimensions of the screen are needed.

use anyhow::{Result, bail};
use image::{GrayImage, Rgb, RgbImage, imageops};
use imageproc::edges::canny;
use imageproc::filter::gaussian_blur_f32;
use imageproc::geometric_transformations::{Border, Interpolation, warp_with};

use crate::libcamera::ScalerCrop;
use crate::screen::Pt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub name: &'static str,
    /// The sensor's pixel array, the coordinate system of `ScalerCrop`.
    sensor: (u32, u32),
    /// Focal length in sensor pixels.
    focal_px: f32,
    pub lambda: f32,
}

/// Camera Module 3 Wide: 2.75 mm lens on the IMX708 (4608 × 2592 at 1.4 µm).
/// Raspberry Pi publishes no distortion data; the default λ is fitted to the
/// datasheet's 102° horizontal field of view, which a 2.75 mm pinhole would
/// only reach at 99°. calibrate_camera measures the real one.
pub const IMX708_WIDE: Profile = Profile {
    name: "Camera Module 3 Wide",
    sensor: (4608, 2592),
    focal_px: 2.75 / 0.0014,
    lambda: -0.0365,
};

/// The profile for a camera named by `libcamera::CameraInfo::model`, if it
/// has a lens that needs correcting.
pub fn for_model(model: &str) -> Option<Profile> {
    model
        .to_ascii_lowercase()
        .contains("imx708_wide")
        .then_some(IMX708_WIDE)
}

impl Profile {
    pub fn with_lambda(self, lambda: f32) -> Self {
        Self { lambda, ..self }
    }

    /// The mapping for one frame: `crop` is the part of the sensor it shows,
    /// as libcamera reported it. Without one the frame is taken to show the
    /// whole sensor, scaled to fit (a different aspect crops the other axis).
    pub fn at(&self, width: u32, height: u32, crop: Option<ScalerCrop>) -> Lens {
        let (w, h) = (width as f32, height as f32);
        let (sw, sh) = (self.sensor.0 as f32, self.sensor.1 as f32);
        match crop {
            Some(c) => {
                let sx = w / c.width as f32;
                let sy = h / c.height as f32;
                Lens::new(
                    (sw / 2.0 - c.x as f32) * sx,
                    (sh / 2.0 - c.y as f32) * sy,
                    self.focal_px * sx,
                    self.lambda,
                )
            }
            None => {
                let s = (w / sw).max(h / sh);
                Lens::new(w / 2.0, h / 2.0, self.focal_px * s, self.lambda)
            }
        }
    }
}

/// A profile applied to one frame: converts between camera pixels and
/// pixels of the corrected frame.
#[derive(Debug, Clone, Copy)]
pub struct Lens {
    /// Optical centre in camera pixels.
    cx: f32,
    cy: f32,
    /// Focal length in camera pixels.
    f: f32,
    /// Focal length in corrected pixels.
    f_out: f32,
    lambda: f32,
}

impl Lens {
    fn new(cx: f32, cy: f32, f: f32, lambda: f32) -> Self {
        // Keep the left edge midpoint where it is.
        let edge = cx / f;
        Self {
            cx,
            cy,
            f,
            f_out: f * (1.0 + lambda * edge * edge),
            lambda,
        }
    }

    pub fn with_lambda(&self, lambda: f32) -> Self {
        Self::new(self.cx, self.cy, self.f, lambda)
    }

    /// The same lens for the frame resized by `k`.
    pub fn scaled(&self, k: f32) -> Self {
        Self::new(self.cx * k, self.cy * k, self.f * k, self.lambda)
    }

    /// Camera pixel → corrected pixel.
    pub fn undistort(&self, p: Pt) -> Pt {
        let (x, y) = ((p.0 - self.cx) / self.f, (p.1 - self.cy) / self.f);
        let s = self.f_out / (1.0 + self.lambda * (x * x + y * y));
        (self.cx + x * s, self.cy + y * s)
    }

    /// Corrected pixel → camera pixel.
    pub fn distort(&self, p: Pt) -> Pt {
        let (x, y) = ((p.0 - self.cx) / self.f_out, (p.1 - self.cy) / self.f_out);
        // ρd from λ ρu ρd² − ρd + ρu = 0, in the form that stays finite at λ = 0.
        let disc = (1.0 - 4.0 * self.lambda * (x * x + y * y)).max(0.0);
        let s = 2.0 * self.f / (1.0 + disc.sqrt());
        (self.cx + x * s, self.cy + y * s)
    }

    /// The frame (which this lens was made for) with the distortion removed.
    pub fn undistort_image(&self, frame: &RgbImage) -> RgbImage {
        warp_with(
            frame,
            |x, y| self.distort((x, y)),
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
        )
    }
}

/// What `measure` found.
#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    pub lambda: f32,
    /// RMS distance of the screen's edge points from straight lines, in
    /// camera pixels: with the measured λ, with the λ it started from, and
    /// with no correction at all.
    pub rms_px: f32,
    pub rms_px_before: f32,
    pub rms_px_uncorrected: f32,
    pub points: usize,
}

impl std::fmt::Display for Measurement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "λ {:.4}: screen edges straight to {:.2} px RMS ({:.2} px before, {:.2} px uncorrected; {} edge points)",
            self.lambda, self.rms_px, self.rms_px_before, self.rms_px_uncorrected, self.points
        )
    }
}

/// Measures λ from the screen in `frame`: finds the LCD with `lens`, gathers
/// the edge pixels along its four sides and picks the λ under which they lie
/// on straight lines. Two rounds, the second starting from the first's λ,
/// so the edge points are gathered close to where they really are.
pub fn measure(frame: &RgbImage, lens: &Lens) -> Result<Measurement> {
    let first = measure_once(frame, lens)?;
    let second = measure_once(frame, &lens.with_lambda(first.lambda))?;
    Ok(Measurement {
        rms_px_before: first.rms_px_before,
        ..second
    })
}

/// Longest side of the image the edge points are taken from.
const MEASURE_SIZE: u32 = 1024;
/// Fewest edge points for a side to count, and fewest sides.
const MIN_POINTS: usize = 30;
const MIN_SIDES: usize = 3;

fn measure_once(frame: &RgbImage, lens: &Lens) -> Result<Measurement> {
    let quad = crate::screen::detect(frame, 0.05, Some(lens))?;
    let k = (MEASURE_SIZE as f32 / frame.width().max(frame.height()) as f32).min(1.0);
    let small = if k < 1.0 {
        imageops::resize(
            frame,
            (frame.width() as f32 * k).round().max(1.0) as u32,
            (frame.height() as f32 * k).round().max(1.0) as u32,
            imageops::FilterType::Triangle,
        )
    } else {
        frame.clone()
    };
    let ls = lens.scaled(k);
    let corners = quad.0.map(|p| (p.0 * k, p.1 * k));

    let gray = gaussian_blur_f32(&imageops::grayscale(&small), 1.2);
    let hi = (1.33 * median(&gray)).clamp(25.0, 90.0);
    let edges = canny(&gray, hi * 0.4, hi);
    let band = (0.012 * small.width().max(small.height()) as f32).max(4.0);

    // Edge pixels near each side, in normalised camera coordinates (centre
    // at the optical axis, unit = focal length), away from the corners.
    let mut sides: [Vec<Pt>; 4] = Default::default();
    for (x, y, p) in edges.enumerate_pixels() {
        if p[0] == 0 {
            continue;
        }
        let u = ls.undistort((x as f32, y as f32));
        let mut best: Option<(usize, f32)> = None;
        for i in 0..4 {
            let (a, b) = (corners[i], corners[(i + 1) % 4]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            if len2 < 1.0 {
                continue;
            }
            let t = ((u.0 - a.0) * dx + (u.1 - a.1) * dy) / len2;
            if !(0.08..=0.92).contains(&t) {
                continue;
            }
            let d = ((u.0 - a.0) * dy - (u.1 - a.1) * dx).abs() / len2.sqrt();
            if d < band && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        if let Some((i, _)) = best {
            sides[i].push(((x as f32 - ls.cx) / ls.f, (y as f32 - ls.cy) / ls.f));
        }
    }

    // Drop what is not the edge itself (UI next to it, the bezel's outer
    // edge): points far from the side's line under the starting λ.
    let sides: Vec<Vec<Pt>> = sides
        .into_iter()
        .map(|pts| trim_outliers(pts, lens.lambda, 1.0 / ls.f))
        .filter(|pts| pts.len() >= MIN_POINTS)
        .collect();
    if sides.len() < MIN_SIDES {
        bail!(
            "found clear edge points on only {} of the screen's sides; show something bright on the device and keep the whole screen in view",
            sides.len()
        );
    }
    let reach = sides
        .iter()
        .flatten()
        .map(|p| (p.0 * p.0 + p.1 * p.1).sqrt())
        .fold(0.0, f32::max);
    if reach < 0.6 {
        bail!(
            "the screen is too small in the frame to measure the lens; move the camera closer so the screen nearly fills the frame"
        );
    }

    let cost = |l: f32| straightness(&sides, l);
    let (mut lo, mut hi) = (-0.30f32, 0.10f32);
    let mut best = lens.lambda;
    let mut best_cost = cost(best);
    let mut l = lo;
    while l <= hi + 1e-6 {
        let c = cost(l);
        if c < best_cost {
            (best, best_cost) = (l, c);
        }
        l += 0.005;
    }
    (lo, hi) = (best - 0.005, best + 0.005);
    let g = 0.618_034f32;
    for _ in 0..24 {
        let (a, b) = (hi - g * (hi - lo), lo + g * (hi - lo));
        if cost(a) < cost(b) {
            hi = b;
        } else {
            lo = a;
        }
    }
    let lambda = (lo + hi) / 2.0;
    // Normalised distances back to (full-size) camera pixels.
    let px = lens.f;
    Ok(Measurement {
        lambda,
        rms_px: cost(lambda) * px,
        rms_px_before: cost(lens.lambda) * px,
        rms_px_uncorrected: cost(0.0) * px,
        points: sides.iter().map(Vec::len).sum(),
    })
}

/// Undistorts normalised camera coordinates with `lambda` (unit focal length).
fn undistort_normalised(p: Pt, lambda: f32) -> Pt {
    let s = 1.0 / (1.0 + lambda * (p.0 * p.0 + p.1 * p.1));
    (p.0 * s, p.1 * s)
}

/// Sum of squared distances from the best-fit line (total least squares),
/// and the residual of each point.
fn line_fit(pts: &[Pt]) -> (f32, Vec<f32>) {
    let n = pts.len() as f32;
    let (mx, my) = pts
        .iter()
        .fold((0.0, 0.0), |(x, y), p| (x + p.0 / n, y + p.1 / n));
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for p in pts {
        let (dx, dy) = (p.0 - mx, p.1 - my);
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    // Direction of the line: the major axis of the scatter.
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let (nx, ny) = (-theta.sin(), theta.cos());
    let res: Vec<f32> = pts
        .iter()
        .map(|p| (p.0 - mx) * nx + (p.1 - my) * ny)
        .collect();
    (res.iter().map(|r| r * r).sum(), res)
}

fn trim_outliers(pts: Vec<Pt>, lambda: f32, min_spread: f32) -> Vec<Pt> {
    if pts.len() < MIN_POINTS {
        return pts;
    }
    let u: Vec<Pt> = pts
        .iter()
        .map(|p| undistort_normalised(*p, lambda))
        .collect();
    let (_, res) = line_fit(&u);
    let mut abs: Vec<f32> = res.iter().map(|r| r.abs()).collect();
    abs.sort_by(f32::total_cmp);
    let limit = (3.0 * abs[abs.len() / 2]).max(min_spread);
    pts.into_iter()
        .zip(res)
        .filter(|(_, r)| r.abs() <= limit)
        .map(|(p, _)| p)
        .collect()
}

/// RMS distance from straight lines of all sides' points under `lambda`.
fn straightness(sides: &[Vec<Pt>], lambda: f32) -> f32 {
    let (mut sum, mut n) = (0.0, 0usize);
    for pts in sides {
        let u: Vec<Pt> = pts
            .iter()
            .map(|p| undistort_normalised(*p, lambda))
            .collect();
        sum += line_fit(&u).0;
        n += pts.len();
    }
    (sum / n.max(1) as f32).sqrt()
}

fn median(img: &GrayImage) -> f32 {
    let mut hist = [0u32; 256];
    for p in img.pixels() {
        hist[p[0] as usize] += 1;
    }
    let half = img.width() * img.height() / 2;
    let mut acc = 0;
    for (v, c) in hist.iter().enumerate() {
        acc += c;
        if acc > half {
            return v as f32;
        }
    }
    255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Pt, b: Pt, tol: f32) -> bool {
        (a.0 - b.0).abs() < tol && (a.1 - b.1).abs() < tol
    }

    #[test]
    fn model_names() {
        assert!(for_model("0 : imx708_wide [4608x2592 10-bit RGGB] (/base/...)").is_some());
        assert!(for_model("0 : imx708_wide_noir [4608x2592 10-bit RGGB]").is_some());
        assert!(for_model("0 : imx708 [4608x2592 10-bit RGGB]").is_none());
        assert!(for_model("0 : imx219 [3280x2464 ...]").is_none());
    }

    #[test]
    fn round_trip_and_fixed_points() {
        for (w, h) in [(4608, 2592), (2304, 1296), (1600, 1200)] {
            let lens = IMX708_WIDE.at(w, h, None);
            let (w, h) = (w as f32, h as f32);
            assert!(close(
                lens.undistort((w / 2.0, h / 2.0)),
                (w / 2.0, h / 2.0),
                1e-3
            ));
            // The left and right edge midpoints stay put.
            assert!(close(lens.undistort((0.0, h / 2.0)), (0.0, h / 2.0), 0.05));
            assert!(close(lens.undistort((w, h / 2.0)), (w, h / 2.0), 0.05));
            for p in [(0.0, 0.0), (w * 0.3, h * 0.9), (w, h), (w * 0.7, h * 0.2)] {
                assert!(close(lens.undistort(lens.distort(p)), p, 0.05), "{p:?}");
                assert!(close(lens.distort(lens.undistort(p)), p, 0.05), "{p:?}");
            }
            // Barrel: the corrected frame's corners lie inside the camera's.
            let c = lens.distort((0.0, 0.0));
            assert!(c.0 > 0.0 && c.1 > 0.0, "{c:?}");
        }
    }

    #[test]
    fn wide_profile_matches_the_datasheet_field_of_view() {
        let lens = IMX708_WIDE.at(4608, 2592, None);
        // Undistorted radius of the edge midpoint, in focal lengths.
        let (x, _) = lens.undistort((4608.0, 1296.0));
        let rho = (x - 2304.0) / lens.f_out;
        let hfov = 2.0 * rho.atan().to_degrees();
        assert!((hfov - 102.0).abs() < 0.5, "{hfov}");
    }

    #[test]
    fn a_cropped_mode_bends_less_than_the_full_sensor() {
        // 1536x864 on the IMX708 is the middle 3072x1728 of the sensor.
        let crop = ScalerCrop {
            x: 768,
            y: 432,
            width: 3072,
            height: 1728,
        };
        let cropped = IMX708_WIDE.at(1536, 864, Some(crop));
        let full = IMX708_WIDE.at(1536, 864, None);
        assert!((cropped.f / full.f - 1.5).abs() < 1e-3);
        // The same sensor pixel comes out the same through either frame.
        let sensor_px = (3500.0f32, 2000.0f32);
        let in_full = (sensor_px.0 / 3.0, sensor_px.1 / 3.0);
        let in_crop = ((sensor_px.0 - 768.0) / 2.0, (sensor_px.1 - 432.0) / 2.0);
        let (a, b) = (full.undistort(in_full), cropped.undistort(in_crop));
        // Both corrections move the point toward the centre by the same angle.
        let ang = |l: &Lens, p: Pt| ((p.0 - l.cx) / l.f_out).atan();
        assert!((ang(&full, a) - ang(&cropped, b)).abs() < 1e-4);
        // Full-sensor crop is the same as no crop.
        let whole = ScalerCrop {
            x: 0,
            y: 0,
            width: 4608,
            height: 2592,
        };
        let w = IMX708_WIDE.at(2304, 1296, Some(whole));
        let n = IMX708_WIDE.at(2304, 1296, None);
        assert!(close(
            w.undistort((10.0, 20.0)),
            n.undistort((10.0, 20.0)),
            1e-3
        ));
    }

    #[test]
    fn zero_lambda_is_identity() {
        let lens = IMX708_WIDE.with_lambda(0.0).at(1000, 600, None);
        for p in [(0.0, 0.0), (123.0, 456.0), (1000.0, 600.0)] {
            assert!(close(lens.undistort(p), p, 1e-3));
            assert!(close(lens.distort(p), p, 1e-3));
        }
    }

    #[test]
    fn line_fit_is_zero_on_a_line() {
        let pts: Vec<Pt> = (0..20).map(|i| (i as f32, 2.0 * i as f32 + 1.0)).collect();
        assert!(line_fit(&pts).0 < 1e-6);
    }
}
