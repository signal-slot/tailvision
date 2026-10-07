//! Keeps the device reachable: when no network is up for a while, raises an
//! open setup hotspot; while the hotspot is up, periodically tries the saved
//! networks again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::led::{Led, Pattern};
use crate::netmgr::{self, AccessPoint};
use crate::{App, tailscale};

#[derive(Default)]
pub struct NetState {
    /// Last scan result, taken before the hotspot goes up (scanning in AP mode is unreliable).
    pub scan_cache: Mutex<Vec<AccessPoint>>,
    pub hotspot_since: Mutex<Option<Instant>>,
    pub last_error: Mutex<Option<String>>,
    /// Set while a connect/hotspot transition is in flight so the loop keeps its hands off.
    pub switching: AtomicBool,
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
    let host = netmgr::hostname().await.unwrap_or_else(|_| "webcam".into());
    let ssid = format!("{host}-setup");
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

/// Joins `ssid` from the web form: drops the hotspot first, restores it on failure.
pub async fn join(app: App, ssid: String, psk: String) {
    app.net.switching.store(true, Ordering::SeqCst);
    // Let the HTTP response reach the browser before the hotspot disappears.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let _ = netmgr::hotspot_down().await;
    match netmgr::connect(&app.cli.wifi_iface, &ssid, &psk).await {
        Ok(()) => {
            tracing::info!(ssid, "joined network");
            *app.net.last_error.lock().await = None;
            *app.net.hotspot_since.lock().await = None;
        }
        Err(e) => {
            tracing::warn!(ssid, error = %e, "join failed; restoring hotspot");
            let _ = netmgr::forget(&ssid).await;
            app.net
                .set_error(format!("joining \"{ssid}\" failed: {e:#}"))
                .await;
            raise_hotspot(&app).await;
        }
    }
    app.net.switching.store(false, Ordering::SeqCst);
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
        let online = netmgr::any_online().await.unwrap_or(false);
        let ts = tailscale::status().await;
        write_status(&app, &mut last_status, &wifi, online, &ts).await;
        if let Some(l) = led.as_mut() {
            l.set(if online {
                if ts.is_running() {
                    Pattern::READY
                } else {
                    Pattern::NO_TAILSCALE
                }
            } else if wifi.is_hotspot() {
                Pattern::HOTSPOT
            } else {
                Pattern::SEARCHING
            });
        }
        if online {
            last_online = Instant::now();
            continue;
        }
        if app.cli.no_hotspot {
            continue;
        }
        if wifi.is_hotspot() {
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
