//! The setup page (`/`) and the access-key checks in front of everything.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine as _;

use crate::App;
use crate::config::{self, Config};
use crate::{netmgr, supervisor, tailscale};

// ------------------------------------------------------------------ auth

/// Gate: `/mcp` and `/shot.jpg` always need the bearer token (or `?key=` for
/// the JPEG). The setup page needs Basic auth with the key as password, except
/// for clients on the setup hotspot: whoever holds its WPA2 password is
/// treated as having physical access, and that is how the key is first read.
pub async fn require_key(State(app): State<App>, req: Request<Body>, next: Next) -> Response {
    let key = app.config.read().await.mcp_key.clone().unwrap_or_default();
    let path = req.uri().path();
    let from_local_link = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| on_hotspot(ci.0.ip()))
        .unwrap_or(false);
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let api = path.starts_with("/mcp") || path.ends_with(".jpg");
    let ok = if api {
        auth.strip_prefix("Bearer ").map(str::trim) == Some(key.as_str())
            || (path == "/shot.jpg"
                && query_param(req.uri().query(), "key").as_deref() == Some(key.as_str()))
    } else {
        from_local_link || basic_password(auth).as_deref() == Some(key.as_str())
    };
    if ok {
        return next.run(req).await;
    }
    if api {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: set Authorization: Bearer <key>\n",
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"tailvision\", charset=\"UTF-8\""),
            )],
            "unauthorized: any user name, the access key as password\n",
        )
            .into_response()
    }
}

/// Clients on the setup hotspot (10.42.0.0/24) may use the setup page without
/// the key: whoever is on it has physical access to the unit. The Pi's own
/// address (.1) is excluded so a request forwarded from elsewhere does not qualify.
fn on_hotspot(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10 && o[1] == 42 && o[2] == 0 && o[3] != 1
        }
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| on_hotspot(v4.into())),
    }
}

fn basic_password(auth: &str) -> Option<String> {
    let b64 = auth.strip_prefix("Basic ")?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let s = String::from_utf8(decoded).ok()?;
    s.split_once(':').map(|(_, p)| p.to_string())
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let q: HashMap<String, String> = serde_urlencoded::from_str(query?).ok()?;
    q.get(name).cloned()
}

// ------------------------------------------------------------------ page

/// The setup page: a static shell. Everything on it comes from `/api/state`,
/// which the page polls, and the `/api/...` actions below, so nothing needs
/// a reload and the page survives the hotspot going away and coming back.
pub async fn index() -> Html<&'static str> {
    Html(include_str!("setup.html"))
}

// ------------------------------------------------------------------ state

#[derive(serde::Serialize)]
pub struct StateJson {
    hostname: String,
    wifi: WifiJson,
    scan: Vec<ApJson>,
    saved: Vec<String>,
    net_error: Option<String>,
    provisioning: Option<ProvisioningJson>,
    tailscale: TailscaleJson,
    camera: CameraJson,
    gadget: bool,
    key: Option<String>,
    urls: UrlsJson,
}

#[derive(serde::Serialize)]
struct WifiJson {
    hotspot: bool,
    state: String,
    ssid: Option<String>,
    ip: Option<String>,
}

#[derive(serde::Serialize)]
struct ApJson {
    ssid: String,
    signal: u8,
    open: bool,
}

#[derive(serde::Serialize)]
struct ProvisioningJson {
    ssid: String,
    ip: Option<String>,
    internet: bool,
    tailscale_running: bool,
    auth_url: Option<String>,
    note: Option<String>,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct TailscaleJson {
    state: String,
    running: bool,
    dns_name: Option<String>,
    ips: Vec<String>,
    auth_url: Option<String>,
    login_running: bool,
    login_error: Option<String>,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct CameraJson {
    backend: String,
    csi: bool,
    lock: Option<String>,
}

#[derive(serde::Serialize)]
struct UrlsJson {
    /// `http://<hostname>.local`, for clients on the same Wi-Fi (mDNS).
    local: String,
    /// `http://<tailnet name>`, or the `.local` one until the unit is on Tailscale.
    tailnet: String,
}

pub async fn api_state(State(app): State<App>) -> Json<StateJson> {
    let hostname = netmgr::hostname().await.unwrap_or_default();
    let wifi = netmgr::wifi_status(&app.cli.wifi_iface)
        .await
        .unwrap_or_default();
    let saved = netmgr::saved_networks().await.unwrap_or_default();
    let scan = app.net.scan_cache.lock().await.clone();
    let net_error = app.net.last_error.lock().await.clone();
    let provisioning = app.net.provisioning.lock().await.clone();
    let ts = tailscale::status().await;
    let login = app.login.view().await;
    let cfg = app.config.read().await.clone();
    let csi = matches!(app.backend, crate::capture::Backend::Csi { .. });
    let local = format!("http://{hostname}.local");
    Json(StateJson {
        urls: UrlsJson {
            tailnet: ts
                .dns_name
                .as_deref()
                .map(|d| format!("http://{d}"))
                .unwrap_or_else(|| local.clone()),
            local,
        },
        wifi: WifiJson {
            hotspot: wifi.is_hotspot(),
            state: wifi.state.clone(),
            ssid: if wifi.is_hotspot() {
                None
            } else {
                wifi.connection.clone()
            },
            ip: wifi.ip.clone(),
        },
        scan: scan
            .into_iter()
            .map(|a| ApJson {
                open: a.security.is_empty(),
                ssid: a.ssid,
                signal: a.signal,
            })
            .collect(),
        saved,
        net_error,
        provisioning: provisioning.map(|p| ProvisioningJson {
            ssid: p.ssid,
            ip: p.ip,
            internet: p.internet,
            tailscale_running: p.tailscale_running,
            auth_url: p.auth_url,
            note: p.note,
            error: p.error,
        }),
        tailscale: TailscaleJson {
            running: ts.is_running(),
            state: ts.backend_state.clone(),
            dns_name: ts.dns_name.clone(),
            ips: ts.ips.clone(),
            auth_url: login.auth_url.clone().or(ts.auth_url.clone()),
            login_running: login.running,
            login_error: login.last_error.clone(),
            error: ts.error.clone(),
        },
        camera: CameraJson {
            backend: match &app.backend {
                crate::capture::Backend::Csi { .. } => format!(
                    "CSI module via libcamera ({})",
                    crate::libcamera::describe()
                        .lines()
                        .last()
                        .unwrap_or("")
                        .trim()
                ),
                crate::capture::Backend::V4l2 => format!("UVC webcam {}", app.cli.device),
            },
            csi,
            lock: cfg.camera_lock.as_ref().map(|l| l.to_string()),
        },
        gadget: app.gadget.is_some(),
        key: cfg.mcp_key,
        hostname,
    })
}

// ------------------------------------------------------------------ actions

#[derive(serde::Serialize)]
pub struct Reply {
    ok: bool,
    message: String,
}

type ActionResult = (StatusCode, Json<Reply>);

fn done(msg: impl Into<String>) -> ActionResult {
    (
        StatusCode::OK,
        Json(Reply {
            ok: true,
            message: msg.into(),
        }),
    )
}

fn fail(msg: impl Into<String>) -> ActionResult {
    (
        StatusCode::BAD_REQUEST,
        Json(Reply {
            ok: false,
            message: msg.into(),
        }),
    )
}

#[derive(serde::Deserialize)]
pub struct WifiForm {
    #[serde(default)]
    ssid: String,
    #[serde(default)]
    psk: String,
}

pub async fn wifi(State(app): State<App>, Json(f): Json<WifiForm>) -> ActionResult {
    let ssid = f.ssid.trim().to_string();
    if ssid.is_empty() {
        return fail("pick a network or type its name");
    }
    tokio::spawn(supervisor::join(app, ssid.clone(), f.psk));
    done(format!(
        "joining \"{ssid}\"; the hotspot goes away for about a minute and comes back with the result"
    ))
}

pub async fn go_online(State(app): State<App>) -> ActionResult {
    if app.net.provisioning.lock().await.is_none() {
        return fail("nothing to go online with; join a network first");
    }
    tokio::spawn(supervisor::go_online(app));
    done("going online; the hotspot goes away now")
}

pub async fn cancel(State(app): State<App>) -> ActionResult {
    supervisor::cancel_provisioning(&app).await;
    done("cancelled")
}

#[derive(serde::Deserialize)]
pub struct SsidForm {
    ssid: String,
}

pub async fn forget(Json(f): Json<SsidForm>) -> ActionResult {
    match netmgr::forget(&f.ssid).await {
        Ok(()) => done(format!("forgot \"{}\"", f.ssid)),
        Err(e) => fail(format!("{e:#}")),
    }
}

pub async fn rescan(State(app): State<App>) -> ActionResult {
    match supervisor::rescan(&app).await {
        Ok(aps) => done(format!("found {} networks", aps.len())),
        Err(e) => fail(format!("scan failed: {e:#}")),
    }
}

#[derive(serde::Deserialize)]
pub struct HostnameForm {
    hostname: String,
}

pub async fn hostname(Json(f): Json<HostnameForm>) -> ActionResult {
    let name = f.hostname.trim();
    if let Err(e) = netmgr::set_hostname(name).await {
        return fail(format!("{e:#}"));
    }
    if tailscale::status().await.is_running()
        && let Err(e) = tailscale::set_hostname(name).await
    {
        return fail(format!("hostname set, but Tailscale rename failed: {e:#}"));
    }
    done(format!(
        "hostname is now {name}; this page is http://{name}.local/"
    ))
}

#[derive(serde::Deserialize)]
pub struct AuthKeyForm {
    authkey: String,
}

pub async fn tailscale_key(State(app): State<App>, Json(f): Json<AuthKeyForm>) -> ActionResult {
    if f.authkey.trim().is_empty() {
        return fail("auth key is empty");
    }
    if !netmgr::any_online().await.unwrap_or(false) {
        // No way out yet (we are probably on the hotspot): keep the key and
        // let the supervisor use it once Wi-Fi is up.
        *app.net.pending_auth_key.lock().await = Some(f.authkey.trim().to_string());
        return done("auth key stored; it is used as soon as the unit is online");
    }
    match tailscale::up_with_key(&f.authkey).await {
        Ok(()) => done("joined the tailnet"),
        Err(e) => fail(format!("{e:#}")),
    }
}

pub async fn tailscale_logout(State(app): State<App>) -> ActionResult {
    match tailscale::logout().await {
        Ok(()) => {
            app.login.reset().await;
            done("logged out of Tailscale")
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

pub async fn tailscale_login(State(app): State<App>) -> ActionResult {
    match app.login.start().await {
        Ok(Some(_)) => done("login link ready"),
        Ok(None) => done("login started; the link appears here in a moment"),
        Err(e) => fail(format!("{e:#}")),
    }
}

async fn save_key(app: &App, key: Option<String>) -> Result<(), String> {
    let cfg = Config {
        mcp_key: key,
        ..app.config.read().await.clone()
    };
    app.store.save(&cfg).map_err(|e| format!("{e:#}"))?;
    *app.config.write().await = cfg;
    Ok(())
}

pub async fn camera_calibrate(State(app): State<App>) -> ActionResult {
    match app.camera.calibrate().await {
        Ok((lock, _)) => done(format!("camera locked: {lock}")),
        Err(e) => fail(format!("calibration failed: {e:#}")),
    }
}

pub async fn camera_unlock(State(app): State<App>) -> ActionResult {
    match app.camera.unlock().await {
        Ok(()) => done("camera back to automatic"),
        Err(e) => fail(format!("{e:#}")),
    }
}

pub async fn key_generate(State(app): State<App>) -> ActionResult {
    let key = match config::generate_key() {
        Ok(k) => k,
        Err(e) => return fail(format!("{e:#}")),
    };
    match save_key(&app, Some(key)).await {
        Ok(()) => done("new key set; the browser will ask for it now"),
        Err(e) => fail(e),
    }
}

// ------------------------------------------------------------------ captive portal

/// Names the operating systems resolve right after joining a network to find
/// out whether it reaches the Internet. dnsmasq on the hotspot answers them
/// with the unit's own address (`netmgr::write_local_dns`), so the probes
/// land in `fallback`. Nothing else is hijacked.
pub const CAPTIVE_PROBE_HOSTS: &[&str] = &[
    "connectivitycheck.gstatic.com",
    "connectivitycheck.android.com",
    "clients3.google.com",
    "captive.apple.com",
    "www.msftconnecttest.com",
    "www.msftncsi.com",
    "detectportal.firefox.com",
    "connectivity-check.ubuntu.com",
    "nmcheck.gnome.org",
    "network-test.debian.org",
    "networkcheck.kde.org",
];

/// The unit's address on the hotspot.
const HOTSPOT_ADDRESS: &str = "10.42.0.1";

/// Where a client on the hotspot is sent for the setup page:
/// `http://<hostname>.local/`, so the address bar shows the unit's name.
/// dnsmasq answers that name on the hotspot (`netmgr::write_local_dns`) and
/// avahi does over mDNS, which is how Apple devices resolve `.local`. The
/// address is the fallback if the hostname cannot be read.
fn portal_url(hostname: Option<&str>) -> String {
    match hostname.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => format!("http://{h}.local/"),
        None => format!("http://{HOTSPOT_ADDRESS}/"),
    }
}

/// The kernel's idea of the hostname, current after `hostnamectl` too.
fn kernel_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
}

/// Anything unknown lands on the setup page. For a client on the hotspot the
/// redirect goes to `http://<hostname>.local/`: that is what makes a phone or
/// laptop open its "sign in to network" window there when its connectivity
/// probe (`CAPTIVE_PROBE_HOSTS`) arrives, and it does not send the browser
/// back to the probe's own host name, which only resolves to the unit while
/// the client stays on the hotspot.
pub async fn fallback(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Redirect {
    if on_hotspot(peer.ip()) {
        Redirect::temporary(&portal_url(kernel_hostname().as_deref()))
    } else {
        Redirect::temporary("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_parsing() {
        let b64 = base64::engine::general_purpose::STANDARD.encode("anyone:s3cret");
        assert_eq!(
            basic_password(&format!("Basic {b64}")).as_deref(),
            Some("s3cret")
        );
        assert_eq!(basic_password("Bearer x"), None);
        assert_eq!(
            query_param(Some("w=1&key=abc"), "key").as_deref(),
            Some("abc")
        );
        assert_eq!(query_param(None, "key"), None);
    }

    #[test]
    fn hotspot_subnet() {
        assert!(on_hotspot("10.42.0.23".parse().unwrap()));
        assert!(on_hotspot("::ffff:10.42.0.23".parse().unwrap()));
        assert!(!on_hotspot("10.42.0.1".parse().unwrap()));
        assert!(!on_hotspot("10.42.1.7".parse().unwrap()));
        assert!(!on_hotspot("100.64.0.9".parse().unwrap()));
        assert!(!on_hotspot("192.168.1.5".parse().unwrap()));
    }

    #[test]
    fn portal_target() {
        assert_eq!(
            portal_url(Some("tailvision-d84d\n")),
            "http://tailvision-d84d.local/"
        );
        assert_eq!(portal_url(None), "http://10.42.0.1/");
        assert_eq!(portal_url(Some("")), "http://10.42.0.1/");
        assert!(CAPTIVE_PROBE_HOSTS.contains(&"captive.apple.com"));
    }
}
