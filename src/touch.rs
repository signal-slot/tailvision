//! Maps points the agent picks on a returned image back onto the device's
//! touch screen (digitizer coordinates 0..TOUCH_MAX on both axes).

use anyhow::{Result, bail};
use imageproc::geometric_transformations::Projection;

use crate::gadget::TOUCH_MAX;
use crate::screen::{Options, Quad};

/// Everything needed to interpret pixel coordinates of the last screenshot.
#[derive(Debug, Clone)]
pub struct ScreenGeometry {
    pub corners: Quad,
    pub frame_size: (u32, u32),
    /// Size of the rectified image as returned (after rotation).
    pub output_size: (u32, u32),
    pub rotation_degrees: i32,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
}

impl ScreenGeometry {
    pub fn new(
        corners: Quad,
        frame_size: (u32, u32),
        output_size: (u32, u32),
        opts: &Options,
    ) -> Self {
        Self {
            corners,
            frame_size,
            output_size,
            rotation_degrees: opts.rotation_degrees.rem_euclid(360),
            flip_horizontal: opts.flip_horizontal,
            flip_vertical: opts.flip_vertical,
        }
    }

    /// Normalised (0..1) screen position for a pixel of the returned screenshot.
    pub fn screenshot_to_screen(&self, x: f32, y: f32) -> Result<(f32, f32)> {
        let (ow, oh) = (self.output_size.0 as f32, self.output_size.1 as f32);
        if !(0.0..=ow).contains(&x) || !(0.0..=oh).contains(&y) {
            bail!(
                "({x}, {y}) is outside the {}x{} screenshot",
                self.output_size.0,
                self.output_size.1
            );
        }
        // Undo the rotation applied last (imageops::rotate90 is clockwise).
        let (w0, h0) = match self.rotation_degrees {
            90 | 270 => (oh, ow),
            _ => (ow, oh),
        };
        let (mut px, mut py) = match self.rotation_degrees {
            90 => (y, h0 - x),
            180 => (w0 - x, h0 - y),
            270 => (w0 - y, x),
            _ => (x, y),
        };
        if self.flip_vertical {
            py = h0 - py;
        }
        if self.flip_horizontal {
            px = w0 - px;
        }
        Ok(((px / w0).clamp(0.0, 1.0), (py / h0).clamp(0.0, 1.0)))
    }

    /// Normalised screen position for a pixel of the raw camera frame.
    pub fn frame_to_screen(&self, x: f32, y: f32) -> Result<(f32, f32)> {
        let (fw, fh) = (self.frame_size.0 as f32, self.frame_size.1 as f32);
        if !(0.0..=fw).contains(&x) || !(0.0..=fh).contains(&y) {
            bail!(
                "({x}, {y}) is outside the {}x{} frame",
                self.frame_size.0,
                self.frame_size.1
            );
        }
        let unit = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let Some(p) = Projection::from_control_points(self.corners.0, unit) else {
            bail!("degenerate screen corners");
        };
        let (u, v) = p * (x, y);
        if !(-0.02..=1.02).contains(&u) || !(-0.02..=1.02).contains(&v) {
            bail!("({x}, {y}) is outside the detected screen");
        }
        Ok((u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)))
    }
}

/// Digitizer coordinates for a normalised position.
pub fn to_digitizer(u: f32, v: f32) -> (u16, u16) {
    (
        (u * TOUCH_MAX as f32).round() as u16,
        (v * TOUCH_MAX as f32).round() as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(rot: i32, fh: bool, fv: bool) -> ScreenGeometry {
        let corners = Quad([
            (100.0, 100.0),
            (500.0, 100.0),
            (500.0, 300.0),
            (100.0, 300.0),
        ]);
        let opts = Options {
            rotation_degrees: rot,
            flip_horizontal: fh,
            flip_vertical: fv,
            ..Default::default()
        };
        let out = if rot % 180 == 0 {
            (400, 200)
        } else {
            (200, 400)
        };
        ScreenGeometry::new(corners, (640, 480), out, &opts)
    }

    #[test]
    fn screenshot_pixels_map_to_screen_fractions() {
        let g = geom(0, false, false);
        assert_eq!(g.screenshot_to_screen(0.0, 0.0).unwrap(), (0.0, 0.0));
        assert_eq!(g.screenshot_to_screen(400.0, 200.0).unwrap(), (1.0, 1.0));
        assert_eq!(g.screenshot_to_screen(100.0, 50.0).unwrap(), (0.25, 0.25));
        assert!(g.screenshot_to_screen(401.0, 0.0).is_err());

        // Rotated 90° clockwise: the screen's top-left ends up top-right.
        let g = geom(90, false, false);
        let (u, v) = g.screenshot_to_screen(200.0, 0.0).unwrap();
        assert!((u - 0.0).abs() < 1e-3 && (v - 0.0).abs() < 1e-3);
        let (u, v) = g.screenshot_to_screen(0.0, 0.0).unwrap();
        assert!((u - 0.0).abs() < 1e-3 && (v - 1.0).abs() < 1e-3);

        let g = geom(0, true, false);
        assert_eq!(g.screenshot_to_screen(0.0, 0.0).unwrap(), (1.0, 0.0));
        let g = geom(180, false, false);
        assert_eq!(g.screenshot_to_screen(0.0, 0.0).unwrap(), (1.0, 1.0));
    }

    #[test]
    fn frame_pixels_go_through_the_homography() {
        let g = geom(0, false, false);
        let (u, v) = g.frame_to_screen(300.0, 200.0).unwrap();
        assert!((u - 0.5).abs() < 1e-3 && (v - 0.5).abs() < 1e-3);
        assert!(g.frame_to_screen(50.0, 50.0).is_err());
        assert_eq!(to_digitizer(0.5, 1.0), (16384, 32767));
    }
}
