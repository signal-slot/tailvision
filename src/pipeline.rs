//! Capture → decode → find the LCD → rectify → JPEG, with an optional cache
//! of the last detected corners for rigs where the camera does not move.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::capture::{self, ShotRequest};
use crate::screen::{self, Options, Quad, Source};
use crate::touch::ScreenGeometry;

#[derive(Debug, Clone, Default)]
pub struct ScreenRequest {
    pub shot: ShotRequest,
    pub options: Options,
    /// Explicit corners in frame pixels; skips detection.
    pub manual_corners: Option<[(f32, f32); 4]>,
    /// Use the previously detected corners (same frame size) instead of detecting again.
    pub reuse_detection: bool,
    /// JPEG quality of the rectified output.
    pub jpeg_quality: u8,
}

pub struct ScreenShot {
    pub jpeg: Vec<u8>,
    pub corners: Quad,
    pub source: Source,
    pub frame_size: (u32, u32),
    pub output_size: (u32, u32),
    pub fourcc: String,
}

/// Last detected corners together with the frame size they belong to, and
/// the geometry of the last screenshot handed out (for touch coordinates).
#[derive(Default)]
pub struct CornerCache(
    Mutex<Option<(Quad, (u32, u32))>>,
    Mutex<Option<ScreenGeometry>>,
);

impl CornerCache {
    pub fn get(&self, frame_size: (u32, u32)) -> Option<Quad> {
        self.0
            .lock()
            .unwrap()
            .filter(|(_, s)| *s == frame_size)
            .map(|(q, _)| q)
    }
    pub fn set(&self, q: Quad, frame_size: (u32, u32)) {
        *self.0.lock().unwrap() = Some((q, frame_size));
    }
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
    pub fn geometry(&self) -> Option<ScreenGeometry> {
        self.1.lock().unwrap().clone()
    }
    fn set_geometry(&self, g: ScreenGeometry) {
        *self.1.lock().unwrap() = Some(g);
    }
}

fn resolve_corners(
    frame: &image::RgbImage,
    req: &ScreenRequest,
    cache: &CornerCache,
) -> Result<(Quad, Source)> {
    let size = frame.dimensions();
    if let Some(pts) = req.manual_corners {
        let (w, h) = (size.0 as f32, size.1 as f32);
        if pts
            .iter()
            .any(|p| p.0 < 0.0 || p.1 < 0.0 || p.0 > w || p.1 > h)
        {
            bail!(
                "manual_corners must lie inside the {}x{} frame",
                size.0,
                size.1
            );
        }
        let q = Quad::ordered(pts);
        cache.set(q, size);
        return Ok((q, Source::Manual));
    }
    if req.reuse_detection
        && let Some(q) = cache.get(size)
    {
        return Ok((q, Source::Cached));
    }
    let q = screen::detect(frame, req.options.min_area_ratio)?;
    cache.set(q, size);
    Ok((q, Source::Auto))
}

/// Blocking: grabs a frame and returns the rectified screen.
pub fn take_screen(req: &ScreenRequest, cache: &CornerCache) -> Result<ScreenShot> {
    let t0 = std::time::Instant::now();
    let shot = capture::take_shot(&req.shot)?;
    let t_capture = t0.elapsed();
    let frame = screen::decode_jpeg(&shot.jpeg).context("decode camera frame")?;
    let t_decode = t0.elapsed() - t_capture;
    let (corners, source) = resolve_corners(&frame, req, cache)?;
    let t_detect = t0.elapsed() - t_capture - t_decode;
    let out = screen::rectify(&frame, corners, &req.options);
    cache.set_geometry(ScreenGeometry::new(
        corners,
        frame.dimensions(),
        out.dimensions(),
        &req.options,
    ));
    let jpeg = screen::encode_jpeg(&out, req.jpeg_quality)?;
    // On a Zero W every stage is seconds, not milliseconds; keep the numbers visible.
    tracing::info!(
        capture_ms = t_capture.as_millis() as u64,
        decode_ms = t_decode.as_millis() as u64,
        detect_ms = t_detect.as_millis() as u64,
        warp_encode_ms = (t0.elapsed() - t_capture - t_decode - t_detect).as_millis() as u64,
        ?source,
        "screenshot"
    );
    Ok(ScreenShot {
        jpeg,
        corners,
        source,
        frame_size: frame.dimensions(),
        output_size: out.dimensions(),
        fourcc: shot.fourcc.to_string(),
    })
}

/// Blocking: grabs a frame and returns it with the detected corners drawn on.
pub fn take_debug(req: &ScreenRequest, cache: &CornerCache) -> Result<ScreenShot> {
    let shot = capture::take_shot(&req.shot)?;
    let frame = screen::decode_jpeg(&shot.jpeg).context("decode camera frame")?;
    let (corners, source) = resolve_corners(&frame, req, cache)?;
    let out = screen::annotate(&frame, &corners, source);
    Ok(ScreenShot {
        jpeg: screen::encode_jpeg(&out, req.jpeg_quality)?,
        corners,
        source,
        frame_size: frame.dimensions(),
        output_size: out.dimensions(),
        fourcc: shot.fourcc.to_string(),
    })
}

pub fn describe(s: &ScreenShot) -> String {
    let c = s.corners.0;
    let fill = s.corners.area() / (s.frame_size.0 as f32 * s.frame_size.1 as f32).max(1.0);
    let hint = if fill < 0.15 {
        " (the screen is small in the frame: move the camera closer or zoom)"
    } else if fill > 0.85 {
        " (the screen nearly fills the frame: make sure no edge is cut off)"
    } else {
        ""
    };
    format!(
        "screen {}x{} from a {}x{} {} frame; corners ({:?}) TL=({:.0},{:.0}) TR=({:.0},{:.0}) BR=({:.0},{:.0}) BL=({:.0},{:.0}); fills {:.0}% of the frame{}; {} bytes JPEG",
        s.output_size.0,
        s.output_size.1,
        s.frame_size.0,
        s.frame_size.1,
        s.fourcc,
        s.source,
        c[0].0,
        c[0].1,
        c[1].0,
        c[1].1,
        c[2].0,
        c[2].1,
        c[3].0,
        c[3].1,
        fill * 100.0,
        hint,
        s.jpeg.len()
    )
}
