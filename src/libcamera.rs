//! Capture through libcamera's `rpicam-still` for the CSI camera modules
//! (Camera Module 2/3, HQ, ...). The ISP, auto-exposure, white balance and
//! autofocus all live in rpicam-apps; we only ask it for one JPEG.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

/// Camera settings measured once and then held fixed, so every screenshot
/// comes out the same and the capture needs no settling time. Auto-exposure
/// and auto white balance misjudge a glowing panel in a dark room, and
/// autofocus hunts on low-contrast UIs; locking removes all three problems.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CameraLock {
    /// Lens position in dioptres (AF modules only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_position: Option<f32>,
    /// Exposure time in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutter_us: Option<u32>,
    /// Analogue gain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain: Option<f32>,
    /// White balance gains (red, blue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awb_gains: Option<(f32, f32)>,
}

impl CameraLock {
    pub fn is_empty(&self) -> bool {
        *self == CameraLock::default()
    }

    fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(l) = self.lens_position {
            a.extend([
                "--autofocus-mode".into(),
                "manual".into(),
                "--lens-position".into(),
                format!("{l:.2}"),
            ]);
        }
        if let Some(s) = self.shutter_us {
            a.extend(["--shutter".into(), s.to_string()]);
        }
        if let Some(g) = self.gain {
            a.extend(["--gain".into(), format!("{g:.3}")]);
        }
        if let Some((r, b)) = self.awb_gains {
            a.extend(["--awbgains".into(), format!("{r:.3},{b:.3}")]);
        }
        a
    }
}

impl std::fmt::Display for CameraLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if let Some(l) = self.lens_position {
            parts.push(format!("lens {l:.2} dpt"));
        }
        if let Some(s) = self.shutter_us {
            parts.push(format!("shutter {:.1} ms", s as f32 / 1000.0));
        }
        if let Some(g) = self.gain {
            parts.push(format!("gain {g:.2}"));
        }
        if let Some((r, b)) = self.awb_gains {
            parts.push(format!("awb r{r:.2} b{b:.2}"));
        }
        if parts.is_empty() {
            f.write_str("auto")
        } else {
            f.write_str(&parts.join(", "))
        }
    }
}

#[derive(Debug, Clone)]
pub struct CsiRequest {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Preview time before the capture, so AE/AWB (and AF) settle.
    pub settle: Duration,
    pub quality: u8,
    /// Ask for an autofocus cycle before the capture (Camera Module 3 and other AF modules only).
    pub autofocus: bool,
    /// Fixed camera settings; when set, no settling is needed and `autofocus` is ignored.
    pub lock: Option<CameraLock>,
    /// Extra arguments appended verbatim (e.g. `--hflip`, `--shutter 20000`).
    pub extra_args: Vec<String>,
}

/// What libcamera found, when rpicam-apps is installed and reports a camera.
#[derive(Debug, Clone)]
pub struct CameraInfo {
    /// First line of `--list-cameras`, e.g. "0 : imx219 [3280x2464 ...] (/base/soc/i2c0mux/i2c@1/imx219@10)".
    pub model: String,
    /// Sensors with a focus motor that libcamera drives.
    pub autofocus: bool,
}

pub fn probe() -> Option<CameraInfo> {
    let out = Command::new("rpicam-still")
        .arg("--list-cameras")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || text.contains("No cameras available") {
        return None;
    }
    let model = text
        .lines()
        .find(|l| l.trim_start().starts_with("0 :"))?
        .trim()
        .to_string();
    let lower = model.to_ascii_lowercase();
    let autofocus = ["imx708", "arducam", "64mp", "16mp", "imx519"]
        .iter()
        .any(|m| lower.contains(m));
    Some(CameraInfo { model, autofocus })
}

/// Describes the cameras libcamera sees, for the setup page and logs.
pub fn describe() -> String {
    match Command::new("rpicam-still").arg("--list-cameras").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(e) => format!("rpicam-still not available: {e}"),
    }
}

/// Grabs one JPEG. Returns (jpeg, width, height).
pub fn take_jpeg(req: &CsiRequest) -> Result<(Vec<u8>, u32, u32)> {
    let (jpeg, w, h, _) = capture(req, false)?;
    Ok((jpeg, w, h))
}

/// Grabs one JPEG with everything on automatic (and an autofocus cycle when
/// the module has one) and returns the settings the camera converged on, so
/// they can be locked for all later captures.
pub fn calibrate(req: &CsiRequest) -> Result<(CameraLock, Vec<u8>)> {
    let auto = CsiRequest {
        lock: None,
        settle: req.settle.max(Duration::from_millis(1500)),
        ..req.clone()
    };
    let (jpeg, _, _, lock) = capture(&auto, true)?;
    let lock = lock.ok_or_else(|| anyhow!("rpicam-still produced no metadata"))?;
    Ok((lock, jpeg))
}

fn capture(
    req: &CsiRequest,
    want_metadata: bool,
) -> Result<(Vec<u8>, u32, u32, Option<CameraLock>)> {
    let mut cmd = Command::new("rpicam-still");
    cmd.args(["--nopreview", "--encoding", "jpg", "--output", "-"]);
    // Locked settings need no convergence time; a short run still lets the
    // sensor deliver a clean frame.
    let settle = if req.lock.as_ref().is_some_and(|l| !l.is_empty()) {
        Duration::from_millis(300)
    } else {
        req.settle
    };
    cmd.args(["--timeout", &settle.as_millis().max(1).to_string()]);
    cmd.args(["--quality", &req.quality.clamp(1, 100).to_string()]);
    if let (Some(w), Some(h)) = (req.width, req.height) {
        cmd.args(["--width", &w.to_string(), "--height", &h.to_string()]);
    }
    match &req.lock {
        Some(l) if !l.is_empty() => {
            cmd.args(l.args());
        }
        _ => {
            if req.autofocus {
                cmd.args(["--autofocus-on-capture"]);
            }
        }
    }
    let meta_path = want_metadata
        .then(|| std::env::temp_dir().join(format!("tailvision-meta-{}.json", std::process::id())));
    if let Some(p) = &meta_path {
        cmd.args([
            "--metadata",
            &p.to_string_lossy(),
            "--metadata-format",
            "json",
        ]);
    }
    cmd.args(&req.extra_args);
    let out = cmd.output().context("run rpicam-still")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        return Err(anyhow!("rpicam-still failed: {last}"));
    }
    let jpeg = out.stdout;
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return Err(anyhow!(
            "rpicam-still produced no JPEG ({} bytes)",
            jpeg.len()
        ));
    }
    let (w, h) = jpeg_size(&jpeg).unwrap_or((req.width.unwrap_or(0), req.height.unwrap_or(0)));
    let lock = meta_path.and_then(|p| {
        let text = std::fs::read_to_string(&p).ok();
        let _ = std::fs::remove_file(&p);
        text.and_then(|t| parse_metadata(&t))
    });
    Ok((jpeg, w, h, lock))
}

/// Pulls the converged settings out of rpicam-still's JSON metadata.
fn parse_metadata(text: &str) -> Option<CameraLock> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
    let awb = v
        .get("ColourGains")
        .and_then(|x| x.as_array())
        .and_then(|a| Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32)));
    Some(CameraLock {
        lens_position: num("LensPosition").map(|x| x as f32),
        shutter_us: num("ExposureTime").map(|x| x.round() as u32),
        gain: num("AnalogueGain").map(|x| x as f32),
        awb_gains: awb,
    })
}

/// Reads the frame size from the SOF marker without decoding.
fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        match marker {
            0xFF => {
                i += 1;
                continue;
            }
            0xD8 | 0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
                let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32;
                return Some((w, h));
            }
            _ => {
                let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
                i += 2 + len;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_and_builds_args() {
        let lock = parse_metadata(
            r#"{"ExposureTime": 19988, "AnalogueGain": 2.5, "ColourGains": [1.81, 1.62], "LensPosition": 4.75, "ColourTemperature": 4800}"#,
        )
        .unwrap();
        assert_eq!(lock.shutter_us, Some(19988));
        assert_eq!(lock.lens_position, Some(4.75));
        assert_eq!(lock.awb_gains, Some((1.81, 1.62)));
        let args = lock.args();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--shutter" && w[1] == "19988")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--awbgains" && w[1] == "1.810,1.620")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--lens-position" && w[1] == "4.75")
        );
        assert_eq!(
            lock.to_string(),
            "lens 4.75 dpt, shutter 20.0 ms, gain 2.50, awb r1.81 b1.62"
        );
        assert!(CameraLock::default().is_empty());
        assert!(parse_metadata("not json").is_none());
    }

    #[test]
    fn reads_sof_size() {
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 80)
            .encode(&[0u8; 12 * 8 * 3], 12, 8, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        assert_eq!(jpeg_size(&jpeg), Some((12, 8)));
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF]), None);
    }
}
