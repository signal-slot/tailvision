//! tailvision: an MCP server that photographs whatever a USB webcam is pointed at.
//!
//! Runs on the machine the camera is plugged into (a Raspberry Pi Zero W, say)
//! and speaks MCP over Streamable HTTP at `/mcp`. A plain `GET /shot.jpg` is
//! served for browsers and curl, and `/` is a setup page for Wi-Fi, hostname,
//! Tailscale and the access key. When no network is reachable the device
//! raises an open `<hostname>-setup` hotspot so the setup page can be reached.

mod capture;
mod config;
mod gadget;
mod led;
mod libcamera;
mod mjpeg;
mod netmgr;
mod pipeline;
mod screen;
mod supervisor;
mod tailscale;
mod touch;
mod v4l2;
mod web;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use clap::Parser;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::{Mutex, RwLock};

use capture::{Shot, ShotRequest};

/// Default hostname, and therefore the default `.local` name, Tailscale name
/// and setup hotspot name (`<hostname>-setup`).
pub const PRODUCT_NAME: &str = "tailvision";
/// Used when no --hotspot-password and no password file is present.
pub const DEFAULT_HOTSPOT_PASSWORD: &str = "tailvision-setup";

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "tailvision: a camera-based KVM for AI agents, served over MCP"
)]
pub struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:80")]
    bind: String,
    /// V4L2 device node.
    #[arg(long, default_value = "/dev/video0")]
    pub device: String,
    /// Default capture width (omit both to use the camera's largest size).
    #[arg(long)]
    width: Option<u32>,
    /// Default capture height.
    #[arg(long)]
    height: Option<u32>,
    /// Frames to discard before keeping one, so auto-exposure settles.
    #[arg(long, default_value_t = 10)]
    skip_frames: u32,
    /// JPEG quality for cameras that only deliver raw YUYV.
    #[arg(long, default_value_t = 85)]
    quality: u8,
    /// Seconds to wait for a single frame before giving up.
    #[arg(long, default_value_t = 5)]
    frame_timeout: u64,
    /// Where the access key is stored.
    #[arg(long, default_value = "/var/lib/tailvision")]
    state_dir: PathBuf,
    /// Wi-Fi interface managed by NetworkManager.
    #[arg(long, default_value = "wlan0")]
    pub wifi_iface: String,
    /// Seconds without any network before the setup hotspot is raised.
    #[arg(long, default_value_t = 60)]
    pub hotspot_after: u64,
    /// Seconds between attempts to leave the hotspot for a saved network.
    #[arg(long, default_value_t = 600)]
    pub hotspot_retry: u64,
    /// WPA2 password of the setup hotspot (8+ characters). Overrides --hotspot-password-file.
    #[arg(long)]
    pub hotspot_password: Option<String>,
    /// File holding the hotspot password, so each unit can ship its own (printed on a label).
    #[arg(long, default_value = "/boot/firmware/hotspot-password")]
    pub hotspot_password_file: PathBuf,
    /// Wi-Fi regulatory domain (ISO 3166-1 alpha-2). Overrides --wifi-country-file.
    #[arg(long)]
    pub wifi_country: Option<String>,
    /// File holding the Wi-Fi country code, written by the image builder.
    #[arg(long, default_value = "/boot/firmware/wifi-country")]
    pub wifi_country_file: PathBuf,
    /// Plain-text status written for offline diagnosis (empty disables).
    #[arg(long, default_value = "/boot/firmware/tailvision-status.txt")]
    pub status_file: Option<PathBuf>,
    /// Never raise the setup hotspot (for development machines).
    #[arg(long)]
    pub no_hotspot: bool,
    /// LED sysfs directory for status blinking (default: /sys/class/leds/ACT, then led0).
    #[arg(long)]
    pub led: Option<PathBuf>,
    /// Leave the activity LED alone.
    #[arg(long)]
    pub no_led: bool,
    /// Camera stack: auto (CSI module if libcamera sees one, else UVC), csi, or uvc.
    #[arg(long, default_value = "auto")]
    pub camera: String,
    /// Milliseconds the CSI camera runs before the capture so exposure and focus settle.
    #[arg(long, default_value_t = 1500)]
    pub csi_settle_ms: u64,
    /// Extra rpicam-still arguments, e.g. "--hflip --vflip" or "--shutter 20000".
    #[arg(long, default_value = "")]
    pub csi_args: String,
    /// Do not set up the USB gadget (touch screen, keyboard, Ethernet) even if a UDC exists.
    #[arg(long)]
    pub no_gadget: bool,
}

#[derive(Clone)]
pub struct App {
    pub cli: Arc<Cli>,
    pub hotspot_password: Arc<String>,
    pub wifi_country: Option<String>,
    pub camera: Camera,
    pub config: Arc<RwLock<config::Config>>,
    pub store: Arc<config::Store>,
    pub net: Arc<supervisor::NetState>,
    pub login: Arc<tailscale::Login>,
    pub corners: Arc<pipeline::CornerCache>,
    pub gadget: Option<Arc<gadget::Gadget>>,
    pub backend: capture::Backend,
}

#[derive(Clone)]
pub struct Camera {
    cli: Arc<Cli>,
    backend: capture::Backend,
    config: Arc<RwLock<config::Config>>,
    store: Arc<config::Store>,
    /// Only one capture at a time: the device cannot be opened twice.
    lock: Arc<Mutex<()>>,
    corners: Arc<pipeline::CornerCache>,
}

impl Camera {
    fn request(
        &self,
        width: Option<u32>,
        height: Option<u32>,
        skip: Option<u32>,
        quality: Option<u8>,
    ) -> ShotRequest {
        ShotRequest {
            backend: match self.backend.clone() {
                capture::Backend::Csi {
                    settle,
                    autofocus,
                    extra_args,
                    ..
                } => capture::Backend::Csi {
                    settle,
                    autofocus,
                    lock: self
                        .config
                        .try_read()
                        .ok()
                        .and_then(|c| c.camera_lock.clone()),
                    extra_args,
                },
                other => other,
            },
            device: self.cli.device.clone(),
            width: width.or(self.cli.width),
            height: height.or(self.cli.height),
            skip_frames: skip.unwrap_or(self.cli.skip_frames),
            quality: quality.unwrap_or(self.cli.quality),
            frame_timeout: Duration::from_secs(self.cli.frame_timeout),
        }
    }

    async fn shoot(&self, req: ShotRequest) -> anyhow::Result<Shot> {
        let _guard = self.lock.lock().await;
        tokio::task::spawn_blocking(move || capture::take_shot(&req))
            .await
            .context("capture task")?
    }

    fn screen_request(
        &self,
        p: &ShotParams,
        defaults: &config::Config,
    ) -> anyhow::Result<pipeline::ScreenRequest> {
        let manual_corners = match &p.manual_corners {
            None => None,
            Some(v) if v.len() == 4 => Some([
                (v[0][0], v[0][1]),
                (v[1][0], v[1][1]),
                (v[2][0], v[2][1]),
                (v[3][0], v[3][1]),
            ]),
            Some(_) => anyhow::bail!("manual_corners needs exactly four [x, y] points"),
        };
        Ok(pipeline::ScreenRequest {
            shot: self.request(p.width, p.height, p.skip_frames, p.quality),
            options: screen::Options {
                min_area_ratio: p.min_area_ratio.unwrap_or(0.05).clamp(0.0, 1.0),
                output_width: p.output_width,
                output_height: p.output_height,
                margin_ratio: p.margin_ratio.unwrap_or(0.0).clamp(-0.5, 0.5),
                rotation_degrees: p.rotation_degrees.unwrap_or(defaults.rotation_degrees),
                flip_horizontal: p.flip_horizontal.unwrap_or(defaults.flip_horizontal),
                flip_vertical: p.flip_vertical.unwrap_or(defaults.flip_vertical),
            },
            manual_corners,
            reuse_detection: p.reuse_detection.unwrap_or(false),
            jpeg_quality: p.quality.unwrap_or(self.cli.quality),
        })
    }

    async fn screen(
        &self,
        req: pipeline::ScreenRequest,
        debug: bool,
    ) -> anyhow::Result<pipeline::ScreenShot> {
        let _guard = self.lock.lock().await;
        let cache = self.corners.clone();
        tokio::task::spawn_blocking(move || {
            if debug {
                pipeline::take_debug(&req, &cache)
            } else {
                pipeline::take_screen(&req, &cache)
            }
        })
        .await
        .context("capture task")?
    }

    /// Measures focus / exposure / white balance on the CSI camera and stores
    /// them as the lock for all later captures. Returns the lock and a preview.
    pub async fn calibrate(&self) -> anyhow::Result<(libcamera::CameraLock, Vec<u8>)> {
        let _guard = self.lock.lock().await;
        let capture::Backend::Csi {
            settle,
            autofocus,
            extra_args,
            ..
        } = self.backend.clone()
        else {
            anyhow::bail!(
                "calibration needs the CSI camera (a UVC webcam keeps its own automatic settings)"
            );
        };
        let req = libcamera::CsiRequest {
            width: self.cli.width,
            height: self.cli.height,
            settle,
            quality: self.cli.quality,
            autofocus,
            lock: None,
            extra_args,
        };
        let (lock, jpeg) = tokio::task::spawn_blocking(move || libcamera::calibrate(&req))
            .await
            .context("calibration task")??;
        let mut cfg = self.config.read().await.clone();
        cfg.camera_lock = Some(lock.clone());
        self.store.save(&cfg)?;
        *self.config.write().await = cfg;
        Ok((lock, jpeg))
    }

    pub async fn unlock(&self) -> anyhow::Result<()> {
        let mut cfg = self.config.read().await.clone();
        cfg.camera_lock = None;
        self.store.save(&cfg)?;
        *self.config.write().await = cfg;
        Ok(())
    }

    async fn formats(&self) -> anyhow::Result<Vec<capture::FormatInfo>> {
        let _guard = self.lock.lock().await;
        let device = self.cli.device.clone();
        tokio::task::spawn_blocking(move || capture::list_formats(&device))
            .await
            .context("format task")?
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct ShotParams {
    /// Return the raw camera frame instead of the detected screen (default false).
    raw: Option<bool>,
    /// Reuse the corners from the previous detection instead of detecting again (default false; faster when the camera does not move).
    reuse_detection: Option<bool>,
    /// Four [x, y] points in raw-frame pixels, any order, overriding detection.
    manual_corners: Option<Vec<[f32; 2]>>,
    /// Width of the rectified screen image; defaults to the detected size.
    output_width: Option<u32>,
    /// Height of the rectified screen image; defaults to the detected size.
    output_height: Option<u32>,
    /// Shrink (positive) or grow (negative) the detected quad toward its centre, as a fraction (-0.5..0.5).
    margin_ratio: Option<f32>,
    /// Rotate the result by 0, 90, 180 or 270 degrees.
    rotation_degrees: Option<i32>,
    flip_horizontal: Option<bool>,
    flip_vertical: Option<bool>,
    /// Smallest candidate area as a fraction of the frame (default 0.05).
    min_area_ratio: Option<f32>,
    /// Camera capture width; must be given together with height. Defaults to the camera's largest size.
    width: Option<u32>,
    /// Camera capture height.
    height: Option<u32>,
    /// Frames to discard before keeping one, so auto-exposure can settle (default 10).
    skip_frames: Option<u32>,
    /// JPEG quality 1-100 of the returned image (default 85).
    quality: Option<u8>,
}

fn csi_backend(cli: &Cli, info: Option<&libcamera::CameraInfo>) -> capture::Backend {
    capture::Backend::Csi {
        settle: Duration::from_millis(cli.csi_settle_ms),
        autofocus: info.is_some_and(|i| i.autofocus),
        lock: None, // filled per request from the saved calibration
        extra_args: cli.csi_args.split_whitespace().map(String::from).collect(),
    }
}

#[derive(Clone)]
struct TailvisionServer {
    camera: Camera,
    gadget: Option<Arc<gadget::Gadget>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct CalibrateParams {
    /// true (default): measure and lock; false: clear the lock and return to automatic.
    lock: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct TapParams {
    /// X in pixels of the last take_screenshot image (or of the raw frame when frame_coords is true).
    x: f32,
    /// Y in pixels of the last take_screenshot image.
    y: f32,
    /// Interpret x/y as raw camera-frame pixels (as in capture_debug) instead of screenshot pixels.
    frame_coords: Option<bool>,
    /// How long the finger stays down, in ms (default 60; use 800+ for a long press).
    hold_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct SwipeParams {
    /// Start X in pixels of the last take_screenshot image.
    x1: f32,
    y1: f32,
    /// End X in pixels of the last take_screenshot image.
    x2: f32,
    y2: f32,
    /// Interpret the points as raw camera-frame pixels instead of screenshot pixels.
    frame_coords: Option<bool>,
    /// Duration of the gesture in ms (default 300).
    duration_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct TextParams {
    /// Printable ASCII text; "\n" presses Enter, "\t" presses Tab. Other characters are skipped.
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default)]
struct KeyParams {
    /// Key name: a single character, or enter, esc, tab, backspace, space, up, down, left, right, home, end, pageup, pagedown, delete, f1..f12.
    key: String,
    /// Modifiers joined with "+", e.g. "ctrl", "ctrl+shift", "alt", "gui".
    modifiers: Option<String>,
}

fn tool_error(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

fn image_result(jpeg: &[u8], note: String) -> CallToolResult {
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
    CallToolResult::success(vec![
        ContentBlock::image(b64, "image/jpeg"),
        ContentBlock::text(note),
    ])
}

#[tool_router]
impl TailvisionServer {
    fn new(camera: Camera, gadget: Option<Arc<gadget::Gadget>>) -> Self {
        Self { camera, gadget }
    }

    fn gadget(&self) -> Result<Arc<gadget::Gadget>, ErrorData> {
        self.gadget.clone().ok_or_else(|| {
            ErrorData::internal_error(
                "USB gadget is not active: the unit's USB port must be in peripheral mode and plugged into the device under test",
                None,
            )
        })
    }

    fn point(&self, x: f32, y: f32, frame_coords: bool) -> Result<(u16, u16), ErrorData> {
        let g = self.camera.corners.geometry().ok_or_else(|| {
            ErrorData::invalid_params(
                "call take_screenshot first so the screen position is known",
                None,
            )
        })?;
        let (u, v) = if frame_coords {
            g.frame_to_screen(x, y)
        } else {
            g.screenshot_to_screen(x, y)
        }
        .map_err(|e| ErrorData::invalid_params(format!("{e:#}"), None))?;
        Ok(touch::to_digitizer(u, v))
    }

    #[tool(
        name = "tap",
        description = "Touch the device's screen at a point given in pixels of the last take_screenshot image (top-left origin). Returns after the finger lifts; take a new screenshot to see the result.",
        annotations(read_only_hint = false, idempotent_hint = false)
    )]
    async fn tap(&self, Parameters(p): Parameters<TapParams>) -> Result<CallToolResult, ErrorData> {
        let g = self.gadget()?;
        let (x, y) = self.point(p.x, p.y, p.frame_coords.unwrap_or(false))?;
        let hold = Duration::from_millis(p.hold_ms.unwrap_or(60).min(10_000));
        tokio::task::spawn_blocking(move || g.tap(x, y, hold))
            .await
            .map_err(|e| tool_error(e.into()))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "tapped ({:.0}, {:.0}) → digitizer ({x}, {y}), held {} ms",
            p.x,
            p.y,
            hold.as_millis()
        ))]))
    }

    #[tool(
        name = "swipe",
        description = "Drag a finger across the device's screen between two points given in pixels of the last take_screenshot image.",
        annotations(read_only_hint = false, idempotent_hint = false)
    )]
    async fn swipe(
        &self,
        Parameters(p): Parameters<SwipeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let g = self.gadget()?;
        let fc = p.frame_coords.unwrap_or(false);
        let from = self.point(p.x1, p.y1, fc)?;
        let to = self.point(p.x2, p.y2, fc)?;
        let duration = Duration::from_millis(p.duration_ms.unwrap_or(300).clamp(50, 10_000));
        tokio::task::spawn_blocking(move || g.swipe(from, to, duration))
            .await
            .map_err(|e| tool_error(e.into()))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "swiped {from:?} → {to:?} over {} ms",
            duration.as_millis()
        ))]))
    }

    #[tool(
        name = "type_text",
        description = "Type text on the device through the USB keyboard (printable ASCII, newline = Enter, tab = Tab).",
        annotations(read_only_hint = false, idempotent_hint = false)
    )]
    async fn type_text(
        &self,
        Parameters(p): Parameters<TextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let g = self.gadget()?;
        let text = p.text.clone();
        let n = tokio::task::spawn_blocking(move || g.type_text(&text))
            .await
            .map_err(|e| tool_error(e.into()))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "typed {n} of {} characters",
            p.text.chars().count()
        ))]))
    }

    #[tool(
        name = "press_key",
        description = "Press one key on the device through the USB keyboard, optionally with modifiers (e.g. key=\"c\", modifiers=\"ctrl\").",
        annotations(read_only_hint = false, idempotent_hint = false)
    )]
    async fn press_key(
        &self,
        Parameters(p): Parameters<KeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let g = self.gadget()?;
        let (mut modifiers, code) = gadget::key_name_to_usage(&p.key)
            .ok_or_else(|| ErrorData::invalid_params(format!("unknown key {:?}", p.key), None))?;
        if let Some(m) = &p.modifiers {
            modifiers |= gadget::parse_modifiers(m)
                .map_err(|e| ErrorData::invalid_params(format!("{e:#}"), None))?;
        }
        tokio::task::spawn_blocking(move || g.key(modifiers, code))
            .await
            .map_err(|e| tool_error(e.into()))?
            .map_err(tool_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "pressed {} (modifiers 0x{modifiers:02x})",
            p.key
        ))]))
    }

    #[tool(
        name = "take_screenshot",
        description = "Photograph the LCD the webcam is pointed at and return just the screen, perspective-corrected, as a JPEG. The screen is auto-detected each time unless reuse_detection or manual_corners is given; pass raw=true for the whole camera frame.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn take_screenshot(
        &self,
        Parameters(p): Parameters<ShotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.raw.unwrap_or(false) {
            let req = self
                .camera
                .request(p.width, p.height, p.skip_frames, p.quality);
            let shot = self.camera.shoot(req).await.map_err(tool_error)?;
            let note = format!(
                "raw {}x{} {} frame, {} bytes JPEG, {} frames skipped",
                shot.width,
                shot.height,
                shot.fourcc,
                shot.jpeg.len(),
                shot.frames_skipped
            );
            return Ok(image_result(&shot.jpeg, note));
        }
        let defaults = self.camera.config.read().await.clone();
        let req = self
            .camera
            .screen_request(&p, &defaults)
            .map_err(tool_error)?;
        let shot = self.camera.screen(req, false).await.map_err(tool_error)?;
        Ok(image_result(&shot.jpeg, pipeline::describe(&shot)))
    }

    #[tool(
        name = "capture_debug",
        description = "Return the raw camera frame with the detected screen outline drawn on it (big dot = top-left), to check or tune the detection.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn capture_debug(
        &self,
        Parameters(p): Parameters<ShotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let defaults = self.camera.config.read().await.clone();
        let req = self
            .camera
            .screen_request(&p, &defaults)
            .map_err(tool_error)?;
        let shot = self.camera.screen(req, true).await.map_err(tool_error)?;
        Ok(image_result(&shot.jpeg, pipeline::describe(&shot)))
    }

    #[tool(
        name = "calibrate_camera",
        description = "Measure focus, exposure and white balance once on the CSI camera and lock them for all later screenshots (consistent images, no settling). Pass lock=false to return to automatic.",
        annotations(read_only_hint = false, idempotent_hint = true)
    )]
    async fn calibrate_camera(
        &self,
        Parameters(p): Parameters<CalibrateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !p.lock.unwrap_or(true) {
            self.camera.unlock().await.map_err(tool_error)?;
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "camera back to automatic focus, exposure and white balance",
            )]));
        }
        let (lock, jpeg) = self.camera.calibrate().await.map_err(tool_error)?;
        Ok(image_result(&jpeg, format!("camera locked: {lock}")))
    }

    #[tool(
        name = "list_formats",
        description = "List the pixel formats and frame sizes the webcam supports.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn list_formats(&self) -> Result<CallToolResult, ErrorData> {
        let formats = self.camera.formats().await.map_err(tool_error)?;
        let text = formats
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for TailvisionServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "take_screenshot returns the device's screen, found in the camera frame and perspective-corrected. \
             Coordinates for tap/swipe are pixels of the last take_screenshot image. \
             capture_debug shows the detected outline on the raw frame; use raw=true or manual_corners if detection is off.",
        )
    }
}

struct HttpError(anyhow::Error);

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}\n", self.0)).into_response()
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}

/// `GET /shot.jpg?w=1280&h=720&skip=10&q=85` (raw frame),
/// `GET /screen.jpg` (detected screen, plus `ow`/`oh`/`reuse=1`), `GET /debug.jpg` (outline drawn).
async fn shot_jpg(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, HttpError> {
    let shot = app
        .camera
        .shoot(params_from_query(&app, &q).await?.shot)
        .await?;
    Ok(jpeg_response(shot.jpeg))
}

async fn screen_jpg(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, HttpError> {
    let shot = app
        .camera
        .screen(params_from_query(&app, &q).await?, false)
        .await?;
    Ok(jpeg_response(shot.jpeg))
}

async fn debug_jpg(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, HttpError> {
    let shot = app
        .camera
        .screen(params_from_query(&app, &q).await?, true)
        .await?;
    Ok(jpeg_response(shot.jpeg))
}

fn jpeg_response(jpeg: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        jpeg,
    )
        .into_response()
}

async fn params_from_query(
    app: &App,
    q: &HashMap<String, String>,
) -> anyhow::Result<pipeline::ScreenRequest> {
    let num = |k: &str| {
        q.get(k)
            .map(|v| {
                v.parse::<u32>()
                    .with_context(|| format!("query parameter {k}"))
            })
            .transpose()
    };
    let p = ShotParams {
        width: num("w")?,
        height: num("h")?,
        skip_frames: num("skip")?,
        quality: num("q")?.map(|v| v.min(100) as u8),
        output_width: num("ow")?,
        output_height: num("oh")?,
        reuse_detection: Some(q.get("reuse").is_some_and(|v| v == "1")),
        ..Default::default()
    };
    let defaults = app.config.read().await.clone();
    app.camera.screen_request(&p, &defaults)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Arc::new(Cli::parse());
    let store = config::Store::new(&cli.state_dir);
    let mut config = store
        .load()
        .with_context(|| format!("load settings from {}", cli.state_dir.display()))?;
    // The MCP endpoint and the camera are never open: mint a key on first start.
    if config.mcp_key.is_none() {
        config.mcp_key = Some(config::generate_key()?);
        store
            .save(&config)
            .context("store the generated access key")?;
        tracing::info!("generated a new access key; read it on the setup page");
    }
    let hotspot_password = match &cli.hotspot_password {
        Some(p) => p.clone(),
        None => std::fs::read_to_string(&cli.hotspot_password_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| DEFAULT_HOTSPOT_PASSWORD.to_string()),
    };
    if !hotspot_password.is_empty() && hotspot_password.len() < 8 {
        anyhow::bail!("hotspot password must be at least 8 characters");
    }
    let wifi_country = cli.wifi_country.clone().or_else(|| {
        std::fs::read_to_string(&cli.wifi_country_file)
            .ok()
            .map(|s| s.trim().to_uppercase())
            .filter(|s| s.len() == 2)
    });
    let corners = Arc::new(pipeline::CornerCache::default());
    let config_shared = Arc::new(RwLock::new(config));
    let store_shared = Arc::new(store);
    let csi = libcamera::probe();
    let backend = match (cli.camera.as_str(), &csi) {
        ("uvc", _) => capture::Backend::V4l2,
        ("csi", _) | ("auto", Some(_)) => csi_backend(&cli, csi.as_ref()),
        _ => capture::Backend::V4l2,
    };
    if let Some(info) = &csi {
        tracing::info!(model = %info.model, autofocus = info.autofocus, "CSI camera");
    }
    tracing::info!(backend = ?backend, "camera");
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| PRODUCT_NAME.into());
    let gadget = if cli.no_gadget {
        None
    } else {
        match gadget::Gadget::setup(PRODUCT_NAME, &hostname) {
            Ok(Some(g)) => Some(Arc::new(g)),
            Ok(None) => {
                tracing::info!(
                    "no USB device controller: gadget mode off (touch/keyboard unavailable)"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "USB gadget setup failed");
                None
            }
        }
    };
    let app = App {
        hotspot_password: Arc::new(hotspot_password),
        wifi_country,
        camera: Camera {
            cli: cli.clone(),
            backend: backend.clone(),
            config: config_shared.clone(),
            store: store_shared.clone(),
            lock: Arc::new(Mutex::new(())),
            corners: corners.clone(),
        },
        cli: cli.clone(),
        config: config_shared.clone(),
        store: store_shared.clone(),
        net: Arc::new(supervisor::NetState::default()),
        login: Arc::new(tailscale::Login::default()),
        corners,
        gadget: gadget.clone(),
        backend,
    };
    if gadget.is_some() {
        tokio::spawn(netmgr::ensure_usb_ethernet());
    }

    // The server is reached through Tailscale, mDNS or the hotspot by whatever
    // name the client likes, so Host-header allow-listing would only get in the way.
    let mcp_config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let mcp = StreamableHttpService::new(
        {
            let camera = app.camera.clone();
            let gadget = app.gadget.clone();
            move || Ok(TailvisionServer::new(camera.clone(), gadget.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );

    let router = axum::Router::new()
        .route("/", get(web::index))
        .route("/shot.jpg", get(shot_jpg))
        .route("/screen.jpg", get(screen_jpg))
        .route("/debug.jpg", get(debug_jpg))
        .route("/setup/wifi", post(web::wifi))
        .route("/setup/forget", post(web::forget))
        .route("/setup/rescan", get(web::rescan))
        .route("/setup/hostname", post(web::hostname))
        .route("/setup/tailscale/key", post(web::tailscale_key))
        .route("/setup/tailscale/login", post(web::tailscale_login))
        .route("/setup/image", post(web::image_defaults))
        .route("/setup/camera/calibrate", post(web::camera_calibrate))
        .route("/setup/camera/unlock", post(web::camera_unlock))
        .route("/setup/key/generate", post(web::key_generate))
        .nest_service("/mcp", mcp)
        .fallback(web::fallback)
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            web::require_key,
        ))
        .with_state(app.clone());

    tokio::spawn(supervisor::run(app.clone()));

    let listener = tokio::net::TcpListener::bind(&cli.bind)
        .await
        .with_context(|| format!("bind {}", cli.bind))?;
    tracing::info!(bind = %cli.bind, device = %cli.device, state = %cli.state_dir.display(), "listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
