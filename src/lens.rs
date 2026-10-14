//! Barrel distortion of wide-angle camera modules, undone so that the rest of
//! the pipeline sees straight screen edges.
//!
//! One-parameter division model around the sensor centre: a point at radius
//! ρd from the optical centre of the camera image (in units of the focal
//! length) belongs at ρu = ρd / (1 + λ ρd²) in a distortion-free image; λ < 0
//! is barrel. The corrected frame has the camera frame's size and keeps the
//! midpoints of its left and right edges, so it shows the same width of the
//! scene; the corners, which barrel distortion squeezes in, are cropped.

use image::{Rgb, RgbImage};
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
/// only reach at 99°. Not yet measured on a real module.
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

    /// The same lens for the frame resized by `k`.
    pub fn scaled(&self, k: f32) -> Self {
        Self::new(self.cx * k, self.cy * k, self.f * k, self.lambda)
    }

    /// Camera pixel → corrected pixel.
    #[cfg_attr(not(test), allow(dead_code))] // only tests map this way so far
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
}
