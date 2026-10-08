//! The setup page (`/`) and the access-key checks in front of everything.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Form, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn back(msg: &str) -> Redirect {
    Redirect::to(&format!(
        "/?msg={}",
        serde_urlencoded::to_string([("m", msg)])
            .unwrap_or_default()
            .trim_start_matches("m=")
    ))
}

pub async fn index(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Html<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("tailvision.local");
    let hostname = netmgr::hostname().await.unwrap_or_default();
    let wifi = netmgr::wifi_status(&app.cli.wifi_iface)
        .await
        .unwrap_or_default();
    let saved = netmgr::saved_networks().await.unwrap_or_default();
    let aps = app.net.scan_cache.lock().await.clone();
    let net_err = app.net.last_error.lock().await.clone();
    let ts = tailscale::status().await;
    let login = app.login.view().await;
    let key = app.config.read().await.mcp_key.clone();

    let mut h = String::with_capacity(8192);
    h.push_str(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>tailvision</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:1rem auto;padding:0 1rem;color:#222}\
         h2{border-bottom:1px solid #ccc;padding-bottom:.2rem;margin-top:2rem}form{margin:.5rem 0}input,select,button{font:inherit;padding:.3rem .5rem;margin:.2rem 0}\
         input[type=text],input[type=password],select{width:100%;box-sizing:border-box}button{cursor:pointer}code,pre{background:#f3f3f3;padding:.1rem .3rem;border-radius:3px}\
         pre{padding:.5rem;overflow-x:auto}.msg{background:#e8f4e8;border:1px solid #9c9;padding:.5rem;border-radius:4px}.err{background:#fbe9e9;border-color:#c99}\
         .row{display:flex;gap:.5rem;align-items:center}.muted{color:#666}</style></head><body>",
    );
    h.push_str(&format!(
        "<h1>tailvision <span class=muted>on {}</span></h1>",
        esc(&hostname)
    ));
    if let Some(m) = q.get("msg") {
        h.push_str(&format!("<p class=msg>{}</p>", esc(m)));
    }
    if let Some(e) = &net_err {
        h.push_str(&format!("<p class='msg err'>Network: {}</p>", esc(e)));
    }

    // ---- result of the last Join, awaiting "Go online"
    if let Some(p) = app.net.provisioning.lock().await.clone() {
        if let Some(e) = &p.error {
            h.push_str(&format!(
                "<div class='msg err'><b>Join failed.</b> {} Check the password and try again below.</div>",
                esc(e)
            ));
        } else if p.tailscale_running {
            h.push_str(&format!(
                "<div class=msg><b>Wi-Fi \"{}\" works</b>{} and the unit is already on Tailscale, so it stays on that network.</div>",
                esc(&p.ssid),
                p.ip.as_deref().map(|ip| format!(" ({})", esc(ip))).unwrap_or_default()
            ));
        } else {
            h.push_str(&format!(
                "<div class=msg><p><b>Wi-Fi \"{}\" works</b>{}. Internet access: <b>{}</b>.</p>",
                esc(&p.ssid),
                p.ip.as_deref()
                    .map(|ip| format!(" ({})", esc(ip)))
                    .unwrap_or_default(),
                if p.internet { "yes" } else { "no" }
            ));
            if let Some(url) = &p.auth_url {
                h.push_str(&format!(
                    "<p>1. Approve the unit by opening this link on a device that is on the Internet. A phone on this hotspot has no Internet, so use another device, or copy the link and open it after leaving the hotspot:<br><a href=\"{0}\">{0}</a></p>",
                    esc(url)
                ));
            }
            if let Some(k) = &key {
                h.push_str(&format!(
                    "<p>2. Note the access key; you need it to register the unit in Claude Code: <code>{}</code></p>",
                    esc(k)
                ));
            }
            if let Some(n) = &p.note {
                h.push_str(&format!("<p class=err>{}</p>", esc(n)));
            }
            h.push_str(
                "<p>3. <form method=post action='/setup/go-online' style='display:inline'><button>Go online</button></form> \
                 The hotspot goes away; the LED turns solid once the unit is on Tailscale. If something is missing, the hotspot returns within two minutes with a note here. \
                 <form method=post action='/setup/cancel' style='display:inline'><button>Cancel</button></form></p></div>",
            );
        }
    }

    // ---- status
    h.push_str("<h2>Status</h2><ul>");
    if wifi.is_hotspot() {
        h.push_str("<li>Wi-Fi: <b>setup hotspot</b> (not connected to any network)</li>");
    } else {
        h.push_str(&format!(
            "<li>Wi-Fi: <b>{}</b>{}{}</li>",
            esc(&wifi.state),
            wifi.connection
                .as_deref()
                .map(|c| format!(" to <b>{}</b>", esc(c)))
                .unwrap_or_default(),
            wifi.ip
                .as_deref()
                .map(|ip| format!(", {}", esc(ip)))
                .unwrap_or_default()
        ));
    }
    h.push_str(&format!(
        "<li>Tailscale: <b>{}</b>{}{}</li>",
        esc(&ts.backend_state),
        ts.dns_name
            .as_deref()
            .map(|d| format!(" as <b>{}</b>", esc(d)))
            .unwrap_or_default(),
        ts.error
            .as_deref()
            .map(|e| format!(" <span class=muted>({})</span>", esc(e)))
            .unwrap_or_default()
    ));
    h.push_str(&format!(
        "<li>Camera: <b>{}</b></li><li>USB to the device under test: <b>{}</b></li>",
        esc(&match &app.backend {
            crate::capture::Backend::Csi { .. } => format!(
                "CSI module via libcamera ({})",
                crate::libcamera::describe()
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
            ),
            crate::capture::Backend::V4l2 => format!("UVC webcam {}", app.cli.device),
        }),
        if app.gadget.is_some() {
            "gadget active (touch, keyboard)"
        } else {
            "off (port in host mode or no cable)"
        }
    ));
    h.push_str(&format!(
        "<li>Access key: <b>{}</b></li>",
        if key.is_some() { "set" } else { "not set" }
    ));
    h.push_str(&format!(
        "<li>Camera: <a href='/screen.jpg{0}'>screen</a> · <a href='/debug.jpg{0}'>detection</a> · <a href='/shot.jpg{0}'>raw frame</a></li></ul>",
        key.as_deref()
            .map(|k| format!("?key={k}"))
            .unwrap_or_default()
    ));

    // ---- wifi
    h.push_str(
        "<h2>Wi-Fi</h2><form method=post action='/setup/wifi'><label>Network<br><select name=ssid>",
    );
    h.push_str("<option value=''>-- type the name below --</option>");
    for ap in &aps {
        h.push_str(&format!(
            "<option value=\"{0}\">{0} ({1}%{2})</option>",
            esc(&ap.ssid),
            ap.signal,
            if ap.security.is_empty() { ", open" } else { "" }
        ));
    }
    h.push_str("</select></label><label>Name (if not listed)<br><input type=text name=ssid_manual autocomplete=off></label>");
    h.push_str("<label>Password<br><input type=password name=psk autocomplete=off></label>");
    h.push_str("<div class=row><button type=submit>Join</button> <a href='/setup/rescan'>rescan</a></div></form>");
    if aps.is_empty() {
        h.push_str(
            "<p class=muted>No scan results yet; use <a href='/setup/rescan'>rescan</a>.</p>",
        );
    }
    if !saved.is_empty() {
        h.push_str("<p>Saved networks:</p><ul>");
        for s in &saved {
            h.push_str(&format!(
                "<li class=row>{} <form method=post action='/setup/forget' style='display:inline'><input type=hidden name=ssid value=\"{}\"><button>forget</button></form></li>",
                esc(s),
                esc(s)
            ));
        }
        h.push_str("</ul>");
    }
    h.push_str("<p class=muted>Join takes the hotspot down for about a minute: the unit joins the network, checks Internet access and fetches a Tailscale login link, then this hotspot comes back and the result appears at the top of this page. (A unit that is already on Tailscale just stays on the new network.)</p>");

    // ---- hostname
    h.push_str(&format!(
        "<h2>Hostname</h2><form method=post action='/setup/hostname'><input type=text name=hostname value=\"{}\" pattern='[A-Za-z0-9-]+'> <button>Rename</button>\
         <p class=muted>Used for <code>.local</code>, the Tailscale name and the hotspot name (<code>&lt;hostname&gt;-setup</code>; until it is changed from the default the hotspot also carries four digits of the unit's Wi-Fi MAC).</p></form>",
        esc(&hostname)
    ));

    // ---- tailscale
    h.push_str("<h2>Tailscale</h2>");
    if ts.is_running() {
        h.push_str(&format!(
            "<p>Logged in as <b>{}</b> ({}).</p>\
             <form method=post action='/setup/tailscale/logout' onsubmit=\"return confirm('Leave the tailnet? The unit is then reachable only on the hotspot until it logs in again.')\"><button>Log out</button> <span class=muted>removes this unit from the tailnet</span></form>",
            esc(ts.dns_name.as_deref().unwrap_or("?")),
            esc(&ts.ips.join(", "))
        ));
    }
    if let Some(url) = login.auth_url.as_deref().or(ts.auth_url.as_deref()) {
        h.push_str(&format!("<p class=msg>Approve the node by opening this link on a device that is on the Internet (not a phone on this hotspot):<br><a href=\"{0}\">{0}</a></p>", esc(url)));
    } else if login.running {
        h.push_str("<p class=msg>Waiting for Tailscale to produce a login link; reload in a few seconds.</p>");
    }
    if let Some(e) = &login.last_error {
        h.push_str(&format!("<p class='msg err'>Login failed: {}</p>", esc(e)));
    }
    h.push_str(
        "<form method=post action='/setup/tailscale/login'><button>Get a login link</button> <span class=muted>needs internet access</span></form>\
         <form method=post action='/setup/tailscale/key'><label>or an auth key<br><input type=password name=authkey placeholder='tskey-auth-...' autocomplete=off></label> <button>Join with key</button></form>",
    );

    // ---- key
    // ---- camera calibration (CSI only)
    if matches!(app.backend, crate::capture::Backend::Csi { .. }) {
        let cfg = app.config.read().await.clone();
        h.push_str("<h2>Camera</h2>");
        match &cfg.camera_lock {
            Some(l) => h.push_str(&format!("<p>Locked: <b>{}</b></p>", esc(&l.to_string()))),
            None => h.push_str("<p>Automatic focus, exposure and white balance (slower, and the picture can change between shots).</p>"),
        }
        h.push_str(
            "<div class=row><form method=post action='/setup/camera/calibrate'><button>Calibrate and lock</button></form>\
             <form method=post action='/setup/camera/unlock'><button>Back to automatic</button></form></div>\
             <p class=muted>Point the camera at the lit screen first. Calibration takes a few seconds; afterwards every screenshot uses the same focus, exposure and white balance.</p>",
        );
    }

    h.push_str("<h2>Access key</h2>");
    match &key {
        Some(k) => {
            let tailnet = ts.dns_name.as_deref().unwrap_or(host);
            h.push_str(&format!(
                "<p>Current key: <code>{k}</code></p><p>Register in Claude Code:</p><pre>claude mcp add --transport http {name} http://{tailnet}/mcp \\\n  --header \"Authorization: Bearer {k}\"</pre>\
                 <p class=muted>Browsers get this page with any user name and the key as password; <code>/screen.jpg?key={k}</code> also works.</p>",
                k = esc(k),
                name = esc(&hostname),
                tailnet = esc(tailnet)
            ));
        }
        None => h.push_str("<p>No key yet; generate one.</p>"),
    }
    h.push_str(
        "<form method=post action='/setup/key/generate'><button>Generate a new key</button> <span class=muted>the old one stops working immediately</span></form>",
    );
    h.push_str("</body></html>");
    Html(h)
}

// ------------------------------------------------------------------ actions

#[derive(serde::Deserialize)]
pub struct WifiForm {
    #[serde(default)]
    ssid: String,
    #[serde(default)]
    ssid_manual: String,
    #[serde(default)]
    psk: String,
}

pub async fn wifi(State(app): State<App>, Form(f): Form<WifiForm>) -> Redirect {
    let ssid = if f.ssid_manual.trim().is_empty() {
        f.ssid
    } else {
        f.ssid_manual.trim().to_string()
    };
    if ssid.is_empty() {
        return back("pick a network or type its name");
    }
    tokio::spawn(supervisor::join(app, ssid.clone(), f.psk));
    back(&format!(
        "joining \"{ssid}\"; the hotspot goes away for about a minute and comes back with the result"
    ))
}

pub async fn go_online(State(app): State<App>) -> Redirect {
    if app.net.provisioning.lock().await.is_none() {
        return back("nothing to go online with; join a network first");
    }
    tokio::spawn(supervisor::go_online(app));
    back("going online; the hotspot goes away now")
}

pub async fn cancel(State(app): State<App>) -> Redirect {
    supervisor::cancel_provisioning(&app).await;
    back("cancelled")
}

#[derive(serde::Deserialize)]
pub struct SsidForm {
    ssid: String,
}

pub async fn forget(Form(f): Form<SsidForm>) -> Redirect {
    match netmgr::forget(&f.ssid).await {
        Ok(()) => back(&format!("forgot \"{}\"", f.ssid)),
        Err(e) => back(&format!("{e:#}")),
    }
}

pub async fn rescan(State(app): State<App>) -> Redirect {
    match supervisor::rescan(&app).await {
        Ok(aps) => back(&format!("found {} networks", aps.len())),
        Err(e) => back(&format!("scan failed: {e:#}")),
    }
}

#[derive(serde::Deserialize)]
pub struct HostnameForm {
    hostname: String,
}

pub async fn hostname(Form(f): Form<HostnameForm>) -> Redirect {
    let name = f.hostname.trim();
    if let Err(e) = netmgr::set_hostname(name).await {
        return back(&format!("{e:#}"));
    }
    if tailscale::status().await.is_running()
        && let Err(e) = tailscale::set_hostname(name).await
    {
        return back(&format!("hostname set, but Tailscale rename failed: {e:#}"));
    }
    back(&format!(
        "hostname is now {name}; reach this page at http://{name}.local/"
    ))
}

#[derive(serde::Deserialize)]
pub struct AuthKeyForm {
    authkey: String,
}

pub async fn tailscale_key(State(app): State<App>, Form(f): Form<AuthKeyForm>) -> Redirect {
    if f.authkey.trim().is_empty() {
        return back("auth key is empty");
    }
    if !netmgr::any_online().await.unwrap_or(false) {
        // No way out yet (we are probably on the hotspot): keep the key and
        // let the supervisor use it once Wi-Fi is up.
        *app.net.pending_auth_key.lock().await = Some(f.authkey.trim().to_string());
        return back("auth key stored; it is used as soon as the unit is online");
    }
    match tailscale::up_with_key(&f.authkey).await {
        Ok(()) => back("joined the tailnet"),
        Err(e) => back(&format!("{e:#}")),
    }
}

pub async fn tailscale_logout(State(app): State<App>) -> Redirect {
    match tailscale::logout().await {
        Ok(()) => {
            app.login.reset().await;
            back("logged out of Tailscale")
        }
        Err(e) => back(&format!("{e:#}")),
    }
}

pub async fn tailscale_login(State(app): State<App>) -> Redirect {
    match app.login.start().await {
        Ok(Some(_)) => back("login link ready"),
        Ok(None) => back("login started; reload for the link"),
        Err(e) => back(&format!("{e:#}")),
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

pub async fn camera_calibrate(State(app): State<App>) -> Redirect {
    match app.camera.calibrate().await {
        Ok((lock, _)) => back(&format!("camera locked: {lock}")),
        Err(e) => back(&format!("calibration failed: {e:#}")),
    }
}

pub async fn camera_unlock(State(app): State<App>) -> Redirect {
    match app.camera.unlock().await {
        Ok(()) => back("camera back to automatic"),
        Err(e) => back(&format!("{e:#}")),
    }
}

pub async fn key_generate(State(app): State<App>) -> Redirect {
    let key = match config::generate_key() {
        Ok(k) => k,
        Err(e) => return back(&format!("{e:#}")),
    };
    match save_key(&app, Some(key)).await {
        Ok(()) => back("new key set; the browser will ask for it now"),
        Err(e) => back(&e),
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

    #[test]
    fn escaping() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
