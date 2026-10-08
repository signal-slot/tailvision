//! Keeps the device reachable: when no network is up for a while, raises the
//! setup hotspot; while the hotspot is up, periodically tries the saved
//! networks again.
//!
//! The Zero W has one radio and cannot run the hotspot and a Wi-Fi client at
//! the same time (its chip cannot do WPA2 on a second virtual interface), so
//! provisioning is a round trip: "Join" drops the hotspot, joins the network,
//! fetches a Tailscale login link while it has Internet, and comes back as
//! the hotspot to report. "Go online" then switches over for good.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::led::{Led, Pattern};
use crate::netmgr::{self, AccessPoint};
use crate::{App, tailscale};

/// Outcome of a "Join" round trip, shown on the setup page once the hotspot
/// is back, until "Go online" succeeds or the user cancels.
#[derive(Clone, Debug, Default)]
pub struct Provisioning {
    pub ssid: String,
    pub wifi_ok: bool,
    pub ip: Option<String>,
    pub internet: bool,
    pub tailscale_running: bool,
    pub auth_url: Option<String>,
    pub error: Option<String>,
    /// Why the last "Go online" came back to the hotspot.
    pub note: Option<String>,
}

#[derive(Default)]
pub struct NetState {
    /// Result of the last Join, awaiting "Go online".
    pub provisioning: Mutex<Option<Provisioning>>,
    /// Tailscale auth key entered while offline (e.g. on the hotspot); applied
    /// by the loop as soon as the unit is online, then dropped.
    pub pending_auth_key: Mutex<Option<String>>,
    /// Last scan result, taken before the hotspot goes up (scanning in AP mode is unreliable).
    pub scan_cache: Mutex<Vec<AccessPoint>>,
    pub hotspot_since: Mutex<Option<Instant>>,
    pub last_error: Mutex<Option<String>>,
    /// Set while a connect/hotspot transition is in flight so the loop keeps its hands off.
    pub switching: AtomicBool,
    /// Last `any_online` result, for answering the connectivity probes of a
    /// device on the USB link (see `web::fallback`).
    pub online: AtomicBool,
}

impl NetState {
    pub async fn set_error(&self, e: impl ToString) {
        *self.last_error.lock().await = Some(e.to_string());
    }
}

pub async fn rescan(app: &App) -> anyhow::Result<Vec<AccessPoint>> {
    let aps = netmgr::scan(&app.cli.wifi_iface).await?;
    *app.net.scan_cache.lock().await = aps.clone();
    Ok(aps)
}

async fn raise_hotspot(app: &App) {
    netmgr::radio_on().await;
    let _ = rescan(app).await;
    let host = netmgr::hostname()
        .await
        .unwrap_or_else(|_| crate::PRODUCT_NAME.into());
    let ssid = hotspot_ssid(&host, &app.cli.wifi_iface);
    match netmgr::hotspot_up(&app.cli.wifi_iface, &ssid, &app.hotspot_password).await {
        Ok(()) => {
            tracing::info!(ssid, "setup hotspot up");
            *app.net.hotspot_since.lock().await = Some(Instant::now());
        }
        Err(e) => {
            tracing::warn!(error = %e, "hotspot failed");
            app.net.set_error(format!("hotspot: {e:#}")).await;
        }
    }
}

/// Last four hex digits of the Wi-Fi MAC: the per-unit suffix printed on the label.
pub fn unit_suffix(wifi_iface: &str) -> Option<String> {
    let mac =
        std::fs::read_to_string(format!("/sys/class/net/{wifi_iface}/address")).unwrap_or_default();
    let digits: String = mac
        .trim()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (digits.len() == 12).then(|| digits[8..].to_string())
}

/// A unit fresh from the image is called just `tailvision`; give it the
/// per-unit name `tailvision-xxxx` once, so its mDNS name, its Tailscale node
/// name and its hotspot (`<hostname>-setup`) are all unique among several
/// units. A hostname the user chose is left alone.
pub async fn ensure_unique_hostname(app: &App) {
    let Ok(host) = netmgr::hostname().await else {
        return;
    };
    if host != crate::PRODUCT_NAME {
        return;
    }
    let Some(suffix) = unit_suffix(&app.cli.wifi_iface) else {
        return;
    };
    let name = format!("{host}-{suffix}");
    match netmgr::set_hostname(&name).await {
        Ok(()) => tracing::info!(hostname = name, "factory hostname made unique"),
        Err(e) => tracing::warn!(error = %e, "could not set the per-unit hostname"),
    }
}

/// `<hostname>-setup`; the hostname itself already carries the unit suffix
/// (see `ensure_unique_hostname`), with a fallback for a unit that could not
/// be renamed.
pub fn hotspot_ssid(host: &str, wifi_iface: &str) -> String {
    if host == crate::PRODUCT_NAME
        && let Some(sfx) = unit_suffix(wifi_iface)
    {
        return format!("{host}-{sfx}-setup");
    }
    format!("{host}-setup")
}

/// "Join" from the web form: a round trip. Drops the hotspot, joins the
/// network, checks Internet access, asks Tailscale for a login link (unless
/// it is already logged in, in which case the unit simply stays online), and
/// raises the hotspot again with the outcome for the page to show.
pub async fn join(app: App, ssid: String, psk: String) {
    app.net.switching.store(true, Ordering::SeqCst);
    // Let the HTTP response reach the browser before the hotspot disappears.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let _ = netmgr::hotspot_down().await;
    let mut p = Provisioning {
        ssid: ssid.clone(),
        ..Default::default()
    };
    match netmgr::connect(&app.cli.wifi_iface, &ssid, &psk).await {
        Ok(()) => {
            tracing::info!(ssid, "joined network");
            *app.net.last_error.lock().await = None;
            p.wifi_ok = true;
            p.ip = netmgr::wifi_status(&app.cli.wifi_iface)
                .await
                .ok()
                .and_then(|w| w.ip);
            p.internet = wait_for(Duration::from_secs(20), netmgr::internet_reachable).await;
            let ts = tailscale::status().await;
            if ts.is_running() {
                // Nothing left to set up: stay on the network.
                p.tailscale_running = true;
                tracing::info!("already on the tailnet; staying online");
                *app.net.hotspot_since.lock().await = None;
                *app.net.provisioning.lock().await = Some(p);
                app.net.switching.store(false, Ordering::SeqCst);
                return;
            }
            if p.internet {
                let _ = app.login.start().await;
                p.auth_url = wait_for_some(Duration::from_secs(40), || async {
                    let v = app.login.view().await;
                    v.auth_url.or(tailscale::status().await.auth_url)
                })
                .await;
                if p.auth_url.is_none() {
                    p.note = Some(
                        "Tailscale did not produce a login link in time; press Go online, or try Join again".into(),
                    );
                }
            } else {
                p.note = Some(
                    "the network has no Internet access (or blocks it); Tailscale cannot be set up from it".into(),
                );
            }
            tracing::info!(
                internet = p.internet,
                link = p.auth_url.is_some(),
                "join done; back to the hotspot to report"
            );
        }
        Err(e) => {
            tracing::warn!(ssid, error = %e, "join failed");
            let _ = netmgr::forget(&ssid).await;
            p.error = Some(format!("joining \"{ssid}\" failed: {e:#}"));
        }
    }
    *app.net.provisioning.lock().await = Some(p);
    raise_hotspot(&app).await;
    app.net.switching.store(false, Ordering::SeqCst);
}

/// "Go online" from the web form: leaves the hotspot for the network joined
/// before and waits for Tailscale to come up (the user approved the link in
/// the meantime). If that does not happen the hotspot comes back with a note.
pub async fn go_online(app: App) {
    let Some(mut p) = app.net.provisioning.lock().await.clone() else {
        return;
    };
    app.net.switching.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let _ = netmgr::hotspot_down().await;
    let ssid = p.ssid.clone();
    if let Err(e) = netmgr::activate(&ssid).await {
        tracing::warn!(ssid, error = %e, "go online: network did not come up");
        p.note = Some(format!("\"{ssid}\" did not come up: {e:#}"));
        *app.net.provisioning.lock().await = Some(p);
        raise_hotspot(&app).await;
        app.net.switching.store(false, Ordering::SeqCst);
        return;
    }
    let running = wait_for(Duration::from_secs(90), || async {
        tailscale::status().await.is_running()
    })
    .await;
    if running {
        tracing::info!("online and on the tailnet; provisioning complete");
        *app.net.provisioning.lock().await = None;
        *app.net.hotspot_since.lock().await = None;
    } else {
        let ts = tailscale::status().await;
        p.auth_url = ts.auth_url.clone().or(p.auth_url);
        p.note = Some(
            "Wi-Fi is up but Tailscale is not logged in yet. Open the link, approve the unit, then press Go online again."
                .into(),
        );
        tracing::info!("go online: Tailscale not running yet; back to the hotspot");
        *app.net.provisioning.lock().await = Some(p);
        raise_hotspot(&app).await;
    }
    app.net.switching.store(false, Ordering::SeqCst);
}

/// Abandons a Join result; the saved network stays saved.
pub async fn cancel_provisioning(app: &App) {
    *app.net.provisioning.lock().await = None;
}

async fn wait_for<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn wait_for_some<T, F, Fut>(limit: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = check().await {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Human-readable state dropped on the boot (FAT) partition, so a unit that
/// never showed up on the network can be diagnosed by putting its card in any
/// PC. Rewritten only when the text changes.
async fn write_status(
    app: &App,
    last: &mut String,
    wifi: &netmgr::WifiStatus,
    online: bool,
    ts: &tailscale::Status,
) {
    let Some(path) = app.cli.status_file.as_ref() else {
        return;
    };
    let err = app.net.last_error.lock().await.clone();
    let text = format!(
        "tailvision status\nhostname: {}\nwifi: {} {}{}\nonline: {}\ntailscale: {} {}\nlast error: {}\n",
        netmgr::hostname().await.unwrap_or_default(),
        wifi.state,
        wifi.connection.as_deref().unwrap_or("-"),
        wifi.ip
            .as_deref()
            .map(|ip| format!(" {ip}"))
            .unwrap_or_default(),
        online,
        ts.backend_state,
        ts.dns_name.as_deref().unwrap_or("-"),
        err.as_deref().unwrap_or("-"),
    );
    if text != *last {
        match tokio::fs::write(path, format!("{text}updated: {}\n", chrono_like_now())).await {
            Ok(()) => *last = text,
            Err(e) => tracing::debug!(error = %e, "status file"),
        }
    }
}

/// Seconds since boot; good enough for ordering without pulling in a date crate.
fn chrono_like_now() -> String {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .map(|u| format!("{u}s after boot"))
        })
        .unwrap_or_default()
}

pub async fn run(app: App) {
    let tick = Duration::from_secs(10);
    let mut last_status = String::new();
    netmgr::set_country(app.wifi_country.as_deref()).await;
    ensure_unique_hostname(&app).await;
    let offline_grace = Duration::from_secs(app.cli.hotspot_after);
    let retry_every = Duration::from_secs(app.cli.hotspot_retry);
    let mut last_online = Instant::now();
    let mut led = if app.cli.no_led {
        None
    } else {
        Led::find(app.cli.led.as_deref())
    };
    match &led {
        Some(_) => tracing::info!("status LED enabled"),
        None if !app.cli.no_led => tracing::info!("no status LED found"),
        None => {}
    }
    if let Some(l) = led.as_mut() {
        l.set(Pattern::SEARCHING);
    }
    loop {
        tokio::time::sleep(tick).await;
        if app.net.switching.load(Ordering::SeqCst) {
            continue;
        }
        let wifi = match netmgr::wifi_status(&app.cli.wifi_iface).await {
            Ok(w) => w,
            Err(e) => {
                tracing::debug!(error = %e, "nmcli unavailable");
                continue;
            }
        };
        let hotspot_up = wifi.is_hotspot();
        let online = netmgr::any_online().await.unwrap_or(false);
        app.net.online.store(online, Ordering::Relaxed);
        let ts = tailscale::status().await;
        write_status(&app, &mut last_status, &wifi, online, &ts).await;
        if let Some(l) = led.as_mut() {
            l.set(if online {
                if ts.is_running() {
                    Pattern::READY
                } else {
                    Pattern::NO_TAILSCALE
                }
            } else if hotspot_up {
                Pattern::HOTSPOT
            } else {
                Pattern::SEARCHING
            });
        }
        // Keep the scan list warm so the setup page has networks to offer the
        // moment the hotspot appears.
        if !online && !hotspot_up && app.net.scan_cache.lock().await.is_empty() {
            let _ = rescan(&app).await;
        }
        if online {
            last_online = Instant::now();
            if !ts.is_running() {
                let key = app.net.pending_auth_key.lock().await.take();
                if let Some(key) = key {
                    match tailscale::up_with_key(&key).await {
                        Ok(()) => tracing::info!("joined the tailnet with the stored auth key"),
                        Err(e) => {
                            tracing::warn!(error = %e, "stored auth key rejected");
                            app.net.set_error(format!("tailscale: {e:#}")).await;
                        }
                    }
                }
            }
            continue;
        }
        if app.cli.no_hotspot {
            continue;
        }
        if wifi.is_hotspot() {
            // While a Join result waits for the user, the hotspot must not
            // wander off to retry networks on its own.
            if app.net.provisioning.lock().await.is_some() {
                continue;
            }
            let since = *app.net.hotspot_since.lock().await;
            let due = since.is_none_or(|t| t.elapsed() >= retry_every);
            if !due
                || netmgr::saved_networks()
                    .await
                    .map(|v| v.is_empty())
                    .unwrap_or(true)
            {
                continue;
            }
            tracing::info!("hotspot: retrying saved networks");
            app.net.switching.store(true, Ordering::SeqCst);
            let _ = netmgr::hotspot_down().await;
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut joined = false;
            while Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if netmgr::any_online().await.unwrap_or(false) {
                    joined = true;
                    break;
                }
            }
            if joined {
                tracing::info!("saved network is back");
                last_online = Instant::now();
                *app.net.hotspot_since.lock().await = None;
            } else {
                raise_hotspot(&app).await;
            }
            app.net.switching.store(false, Ordering::SeqCst);
        } else if last_online.elapsed() >= offline_grace && wifi.state != "missing" {
            tracing::info!(
                "no network for {:?}; raising setup hotspot",
                last_online.elapsed()
            );
            app.net.switching.store(true, Ordering::SeqCst);
            raise_hotspot(&app).await;
            app.net.switching.store(false, Ordering::SeqCst);
        }
    }
}
