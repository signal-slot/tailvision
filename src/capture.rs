//! Single-frame capture from a V4L2 (UVC) camera, returned as a JPEG.
//!
//! MJPEG cameras hand us a JPEG directly (after a DHT fix-up); YUYV cameras
//! are encoded here with `jpeg-encoder`. Everything is blocking and meant to
//! be run on a blocking thread.

use std::fmt;
use std::io;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::mjpeg;
use crate::v4l2::{Device, FourCC};

pub const MJPG: FourCC = FourCC(*b"MJPG");
pub const YUYV: FourCC = FourCC(*b"YUYV");
/// Marker for frames that came from libcamera rather than V4L2.
pub const CSI: FourCC = FourCC(*b"CSI ");

/// Which camera stack grabs the frame.
#[derive(Debug, Clone, Default)]
pub enum Backend {
    /// UVC webcam through V4L2 (`device`).
    #[default]
    V4l2,
    /// CSI camera module through libcamera (`rpicam-still`).
    Csi {
        settle: Duration,
        autofocus: bool,
        lock: Option<crate::libcamera::CameraLock>,
        extra_args: Vec<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ShotRequest {
    pub backend: Backend,
    pub device: String,
    /// Desired size; `None` picks the largest the camera offers.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Frames to discard before keeping one, so auto-exposure can settle.
    pub skip_frames: u32,
    /// JPEG quality used only when the camera delivers raw YUYV.
    pub quality: u8,
    /// Give up if the camera does not deliver a frame in this time.
    pub frame_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Shot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub fourcc: FourCC,
    pub frames_skipped: u32,
}

#[derive(Debug, Clone)]
pub struct FormatInfo {
    pub fourcc: FourCC,
    pub description: String,
    pub sizes: Vec<(u32, u32)>,
}

impl fmt::Display for FormatInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.fourcc, self.description)?;
        for (w, h) in &self.sizes {
            write!(f, "\n  {w}x{h}")?;
        }
        Ok(())
    }
}

/// Lists every pixel format and discrete frame size the device offers.
pub fn list_formats(device: &str) -> Result<Vec<FormatInfo>> {
    let dev = Device::open(device).with_context(|| format!("open {device}"))?;
    formats_of(&dev)
}

fn formats_of(dev: &Device) -> Result<Vec<FormatInfo>> {
    let mut out = Vec::new();
    for desc in dev.enum_formats().context("enumerate formats")? {
        let mut sizes = dev.enum_framesizes(desc.fourcc).unwrap_or_default();
        sizes.sort_unstable();
        sizes.dedup();
        out.push(FormatInfo {
            fourcc: desc.fourcc,
            description: desc.description,
            sizes,
        });
    }
    Ok(out)
}

/// Picks the frame size: exact match if offered, otherwise the largest size
/// that fits inside the request, otherwise the largest size at all.
fn choose_size(sizes: &[(u32, u32)], want: Option<(u32, u32)>) -> Option<(u32, u32)> {
    let largest =
        |it: &mut dyn Iterator<Item = &(u32, u32)>| it.max_by_key(|(w, h)| w * h).copied();
    match want {
        Some(w) if sizes.contains(&w) => Some(w),
        Some((ww, wh)) => largest(&mut sizes.iter().filter(|(w, h)| *w <= ww && *h <= wh))
            .or_else(|| largest(&mut sizes.iter())),
        None => largest(&mut sizes.iter()),
    }
}

/// Grabs one frame and returns it as JPEG.
pub fn take_shot(req: &ShotRequest) -> Result<Shot> {
    if let Backend::Csi {
        settle,
        autofocus,
        lock,
        extra_args,
    } = &req.backend
    {
        let csi = crate::libcamera::CsiRequest {
            width: req.width,
            height: req.height,
            settle: *settle,
            quality: req.quality,
            autofocus: *autofocus,
            lock: lock.clone(),
            extra_args: extra_args.clone(),
        };
        let (jpeg, width, height) = crate::libcamera::take_jpeg(&csi)?;
        return Ok(Shot {
            jpeg,
            width,
            height,
            fourcc: CSI,
            frames_skipped: 0,
        });
    }
    let dev = Device::open(&req.device).with_context(|| format!("open {}", req.device))?;
    let formats = formats_of(&dev)?;
    let chosen = formats
        .iter()
        .find(|f| f.fourcc == MJPG)
        .or_else(|| formats.iter().find(|f| f.fourcc == YUYV))
        .ok_or_else(|| {
            anyhow!(
                "camera offers neither MJPG nor YUYV (has: {})",
                formats
                    .iter()
                    .map(|f| f.fourcc.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let want = match (req.width, req.height) {
        (Some(w), Some(h)) => Some((w, h)),
        (None, None) => None,
        _ => bail!("width and height must be given together"),
    };
    let (w, h) =
        choose_size(&chosen.sizes, want).ok_or_else(|| anyhow!("camera reports no frame sizes"))?;

    let actual = dev
        .set_format(w, h, chosen.fourcc)
        .with_context(|| format!("set format {}x{} {}", w, h, chosen.fourcc))?;
    let actual_fourcc = FourCC(actual.pixelformat.to_le_bytes());
    if actual_fourcc != chosen.fourcc {
        bail!("driver changed pixel format to {actual_fourcc}");
    }

    let mut stream = dev.start_stream(4).context("start stream")?;
    let mut frames_skipped = 0;
    let frame = loop {
        let frame = stream
            .next_frame(req.frame_timeout, |data| data.to_vec())
            .map_err(|e| timeout_hint(e, req.frame_timeout))?;
        // UVC drivers occasionally deliver an empty or truncated first buffer; skip those too.
        if frames_skipped >= req.skip_frames && !frame.is_empty() {
            break frame;
        }
        frames_skipped += 1;
    };
    drop(stream);

    let jpeg = if actual_fourcc == MJPG {
        mjpeg::ensure_dht(&frame)
    } else {
        encode_yuyv(
            &frame,
            actual.width,
            actual.height,
            actual.bytesperline,
            req.quality,
        )?
    };

    Ok(Shot {
        jpeg,
        width: actual.width,
        height: actual.height,
        fourcc: actual_fourcc,
        frames_skipped,
    })
}

fn timeout_hint(e: io::Error, timeout: Duration) -> anyhow::Error {
    if e.kind() == io::ErrorKind::TimedOut {
        anyhow!("no frame within {timeout:?}; is the camera busy or unplugged?")
    } else {
        anyhow::Error::from(e).context("dequeue frame")
    }
}

/// Packs interleaved YUYV (4 bytes per 2 pixels) into YCbCr (3 bytes per pixel)
/// and encodes it with `jpeg-encoder`, which takes YCbCr input directly.
fn encode_yuyv(frame: &[u8], width: u32, height: u32, stride: u32, quality: u8) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 {
        bail!("unsupported YUYV frame size {w}x{h}");
    }
    let stride = if stride == 0 { w * 2 } else { stride as usize };
    if frame.len() < stride * (h - 1) + w * 2 {
        bail!(
            "short YUYV frame: {} bytes for {w}x{h} (stride {stride})",
            frame.len()
        );
    }
    let mut ycbcr = Vec::with_capacity(w * h * 3);
    for row in frame.chunks(stride).take(h) {
        for px in row[..w * 2].as_chunks::<4>().0 {
            let (y0, cb, y1, cr) = (px[0], px[1], px[2], px[3]);
            ycbcr.extend_from_slice(&[y0, cb, cr, y1, cb, cr]);
        }
    }
    let mut out = Vec::with_capacity(w * h / 4);
    let w16 = u16::try_from(w).context("width exceeds JPEG limit")?;
    let h16 = u16::try_from(h).context("height exceeds JPEG limit")?;
    jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100))
        .encode(&ycbcr, w16, h16, jpeg_encoder::ColorType::Ycbcr)
        .context("encode jpeg")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_selection() {
        let sizes = [(160, 120), (640, 480), (1280, 720), (1920, 1080)];
        assert_eq!(choose_size(&sizes, None), Some((1920, 1080)));
        assert_eq!(choose_size(&sizes, Some((640, 480))), Some((640, 480)));
        assert_eq!(choose_size(&sizes, Some((800, 600))), Some((640, 480)));
        assert_eq!(choose_size(&sizes, Some((100, 100))), Some((1920, 1080)));
        assert_eq!(choose_size(&[], None), None);
    }

    #[test]
    fn yuyv_encodes_with_padding_stride() {
        let (w, h, stride) = (4u32, 2u32, 12u32);
        let mut frame = vec![0u8; (stride * h) as usize];
        for row in frame.chunks_mut(stride as usize) {
            for px in row[..8].chunks_mut(4) {
                px.copy_from_slice(&[200, 128, 100, 128]);
            }
        }
        let jpeg = encode_yuyv(&frame, w, h, stride, 80).unwrap();
        let px = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8()
            .into_raw();
        assert_eq!(px.len(), (w * h * 3) as usize);
        assert!(encode_yuyv(&frame[..10], w, h, stride, 80).is_err());
    }
}
