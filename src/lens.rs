//! Barrel distortion of wide-angle camera modules, undone so that the rest of
//! the pipeline sees straight screen edges.
//!
//! One-parameter division model around the frame centre: a point at radius
//! ρd from the centre of the camera image (in units of the focal length)
//! belongs at ρu = ρd / (1 + λ ρd²) in a distortion-free image; λ < 0 is
//! barrel. The corrected frame has the camera frame's size and keeps the
//! midpoints of its left and right edges, so it shows the same width of the
//! scene; the corners, which barrel distortion squeezes in, are cropped.

use image::{Rgb, RgbImage};
use imageproc::geometric_transformations::{Border, Interpolation, warp_with};

use crate::screen::Pt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub name: &'static str,
    /// Focal length over the width and the height of the sensor's full field
    /// of view; a frame cropped to another aspect keeps one of the two.
    focal_per_width: f32,
    focal_per_height: f32,
    pub lambda: f32,
}

/// Camera Module 3 Wide: 2.75 mm lens on a 6.45 × 3.63 mm IMX708
/// (4608 × 2592 at 1.4 µm). Raspberry Pi publishes no distortion data; λ is
/// fitted to the datasheet's 102° horizontal field of view, which a 2.75 mm
/// pinhole would only reach at 99°. Not yet measured on a real module.
pub const IMX708_WIDE: Profile = Profile {
    name: "Camera Module 3 Wide",
    focal_per_width: 2.75 / 6.4512,
    focal_per_height: 2.75 / 3.6288,
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

    /// The mapping for frames of this size.
    pub fn at(&self, width: u32, height: u32) -> Lens {
        let (w, h) = (width as f32, height as f32);
        let f = (w * self.focal_per_width).max(h * self.focal_per_height);
        let edge = w / 2.0 / f;
        Lens {
            cx: w / 2.0,
            cy: h / 2.0,
            f,
            f_out: f * (1.0 + self.lambda * edge * edge),
            lambda: self.lambda,
        }
    }

    /// The frame with the distortion removed, same size.
    pub fn undistort_image(&self, frame: &RgbImage) -> RgbImage {
        let lens = self.at(frame.width(), frame.height());
        warp_with(
            frame,
            |x, y| lens.distort((x, y)),
            Interpolation::Bilinear,
            Border::Constant(Rgb([0, 0, 0])),
        )
    }
}

/// A profile applied to one frame size: converts between camera pixels and
/// pixels of the corrected frame.
#[derive(Debug, Clone, Copy)]
pub struct Lens {
    cx: f32,
    cy: f32,
    /// Focal length in camera pixels.
    f: f32,
    /// Focal length in corrected pixels.
    f_out: f32,
    lambda: f32,
}

impl Lens {
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
            let lens = IMX708_WIDE.at(w, h);
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
        let lens = IMX708_WIDE.at(4608, 2592);
        // Undistorted radius of the edge midpoint, in focal lengths.
        let (x, _) = lens.undistort((4608.0, 1296.0));
        let rho = (x - 2304.0) / lens.f_out;
        let hfov = 2.0 * rho.atan().to_degrees();
        assert!((hfov - 102.0).abs() < 0.5, "{hfov}");
    }

    #[test]
    fn zero_lambda_is_identity() {
        let lens = IMX708_WIDE.with_lambda(0.0).at(1000, 600);
        for p in [(0.0, 0.0), (123.0, 456.0), (1000.0, 600.0)] {
            assert!(close(lens.undistort(p), p, 1e-3));
            assert!(close(lens.distort(p), p, 1e-3));
        }
    }
}
