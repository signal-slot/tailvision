//! Capture → decode → find the LCD → rectify → JPEG, with an optional cache
//! of the last detected corners for rigs where the camera does not move.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::capture::{self, ShotRequest};
use crate::lens::Lens;
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
    /// The camera's lens, when its distortion needs undoing.
    pub lens: Option<crate::lens::Profile>,
    /// Which of several screens in view (1 = leftmost); None takes the
    /// single most screen-like one.
    pub screen: Option<usize>,
    /// A rough box [x0, y0, x1, y1] around the wanted screen, in frame
    /// pixels (corrected with a lens); the screen is detected inside it.
    pub region: Option<[f32; 4]>,
}

pub struct ScreenShot {
    pub jpeg: Vec<u8>,
    pub corners: Quad,
    pub source: Source,
    pub frame_size: (u32, u32),
    pub output_size: (u32, u32),
    pub fourcc: String,
    /// The lens distortion that was undone, if any.
    pub lens: Option<&'static str>,
    /// The screen taken, and how many were found, when it was picked by number.
    pub screen: Option<(usize, usize)>,
    /// How many screens capture_debug found (it numbers them all).
    pub screens_found: Option<usize>,
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

/// The screen asked for by number out of all those found.
fn pick(all: &[Quad], n: usize) -> Result<Quad> {
    match n.checked_sub(1).and_then(|i| all.get(i)) {
        Some(q) => Ok(*q),
        None => bail!(
            "screen {n} not found: {} screen(s) in view, numbered 1..{} from the left (see capture_debug)",
            all.len(),
            all.len()
        ),
    }
}

/// The corners, how they were found, and (number, count) when picked by number.
type Resolved = (Quad, Source, Option<(usize, usize)>);

/// The corners to use, how they were found, and the screen number and
/// count when picked by number. `all` is every screen found, when the caller
/// already has them.
fn resolve_corners(
    frame: &image::RgbImage,
    req: &ScreenRequest,
    lens: Option<&Lens>,
    cache: &CornerCache,
    all: Option<&[Quad]>,
) -> Result<Resolved> {
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
        return Ok((q, Source::Manual, None));
    }
    if req.reuse_detection
        && let Some(q) = cache.get(size)
    {
        return Ok((q, Source::Cached, None));
    }
    if let Some(r) = req.region {
        let q = screen::detect_in(frame, r, lens)?;
        cache.set(q, size);
        return Ok((q, Source::Auto, None));
    }
    if let Some(n) = req.screen {
        let found;
        let all = match all {
            Some(a) => a,
            None => {
                found = screen::detect_all(frame, req.options.min_area_ratio, lens)?;
                &found
            }
        };
        let q = pick(all, n)?;
        cache.set(q, size);
        return Ok((q, Source::Auto, Some((n, all.len()))));
    }
    let q = screen::detect(frame, req.options.min_area_ratio, lens)?;
    cache.set(q, size);
    Ok((q, Source::Auto, None))
}

/// The lens mapping for this frame: its size, and the part of the sensor it shows.
fn frame_lens(req: &ScreenRequest, shot: &capture::Shot, frame: &image::RgbImage) -> Option<Lens> {
    req.lens
        .map(|p| p.at(frame.width(), frame.height(), shot.crop))
}

/// Blocking: grabs a frame and returns the rectified screen.
pub fn take_screen(req: &ScreenRequest, cache: &CornerCache) -> Result<ScreenShot> {
    let t0 = std::time::Instant::now();
    let shot = capture::take_shot(&req.shot)?;
    let t_capture = t0.elapsed();
    let frame = screen::decode_jpeg(&shot.jpeg).context("decode camera frame")?;
    let t_decode = t0.elapsed() - t_capture;
    let lens = frame_lens(req, &shot, &frame);
    let (corners, source, picked) = resolve_corners(&frame, req, lens.as_ref(), cache, None)?;
    let t_detect = t0.elapsed() - t_capture - t_decode;
    let out = screen::rectify(&frame, corners, &req.options, lens.as_ref());
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
        lens: req.lens.map(|l| l.name),
        screen: picked,
        screens_found: None,
    })
}

/// Blocking: grabs a frame and returns it with every screen found drawn on
/// and numbered, and the corners take_screenshot would use on top.
pub fn take_debug(req: &ScreenRequest, cache: &CornerCache) -> Result<ScreenShot> {
    let shot = capture::take_shot(&req.shot)?;
    let frame = screen::decode_jpeg(&shot.jpeg).context("decode camera frame")?;
    let lens = frame_lens(req, &shot, &frame);
    let all =
        screen::detect_all(&frame, req.options.min_area_ratio, lens.as_ref()).unwrap_or_default();
    let (corners, source, picked) = resolve_corners(&frame, req, lens.as_ref(), cache, Some(&all))?;
    // Corners live in the corrected frame, so that is the one to draw on.
    let frame = match &lens {
        Some(l) => l.undistort_image(&frame),
        None => frame,
    };
    let out = screen::annotate(&frame, &corners, source, &all);
    Ok(ScreenShot {
        jpeg: screen::encode_jpeg(&out, req.jpeg_quality)?,
        corners,
        source,
        frame_size: frame.dimensions(),
        output_size: out.dimensions(),
        fourcc: shot.fourcc.to_string(),
        lens: req.lens.map(|l| l.name),
        screen: picked,
        screens_found: Some(all.len()),
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
        "screen {}x{} from a {}x{} {} frame{}; corners ({:?}) TL=({:.0},{:.0}) TR=({:.0},{:.0}) BR=({:.0},{:.0}) BL=({:.0},{:.0}); fills {:.0}% of the frame{}; {} bytes JPEG",
        s.output_size.0,
        s.output_size.1,
        s.frame_size.0,
        s.frame_size.1,
        s.fourcc,
        s.lens
            .map(|l| format!(" corrected for the {l} lens"))
            .unwrap_or_default(),
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
    ) + &match (s.screen, s.screens_found) {
        (Some((n, k)), _) => format!("; screen {n} of {k} in view (numbered from the left)"),
        (None, Some(k)) if k > 1 => format!(
            "; {k} screens in view, outlined in yellow and numbered from the left: pass screen=N to take_screenshot for one of them"
        ),
        _ => String::new(),
    }
}
