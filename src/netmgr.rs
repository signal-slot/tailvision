//! NetworkManager control through `nmcli`: Wi-Fi status, scanning, joining a
//! network, and the open setup hotspot used when no network is reachable.

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

pub const HOTSPOT_CONNECTION: &str = "tailvision-hotspot";
pub const HOTSPOT_ADDRESS: &str = "10.42.0.1";

#[derive(Debug, Clone, Default)]
pub struct WifiStatus {
    /// `connected`, `disconnected`, `connecting`, `unavailable`, ...
    pub state: String,
    /// Active connection profile name (the SSID for networks we add).
    pub connection: Option<String>,
    pub ip: Option<String>,
}

impl WifiStatus {
    pub fn is_hotspot(&self) -> bool {
        self.connection.as_deref() == Some(HOTSPOT_CONNECTION)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: String,
    pub signal: u8,
    pub security: String,
}

async fn nmcli(args: &[&str]) -> Result<String> {
    let out = Command::new("nmcli")
        .args(args)
        .output()
        .await
        .context("run nmcli")?;
    if !out.status.success() {
        return Err(anyhow!(
            "nmcli {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Splits one line of `nmcli -t` output, honouring its `\:` escaping.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    fields.last_mut().unwrap().push(n);
                }
            }
            ':' => fields.push(String::new()),
            _ => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

pub async fn wifi_status(iface: &str) -> Result<WifiStatus> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"]).await?;
    let mut status = WifiStatus {
        state: "missing".into(),
        ..Default::default()
    };
    for line in out.lines() {
        let f = split_terse(line);
        if f.len() >= 4 && f[0] == iface {
            status.state = f[2].clone();
            status.connection = (!f[3].is_empty()).then(|| f[3].clone());
        }
    }
    if status.state == "connected" {
        let out = nmcli(&["-t", "-f", "IP4.ADDRESS", "dev", "show", iface])
            .await
            .unwrap_or_default();
        status.ip = out.lines().find_map(|l| {
            let f = split_terse(l);
            (f.len() >= 2).then(|| f[1].split('/').next().unwrap_or("").to_string())
        });
    }
    Ok(status)
}

/// Whether any interface (Ethernet, USB adapter, ...) other than the hotspot is up.
pub async fn any_online() -> Result<bool> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"]).await?;
    Ok(out.lines().map(split_terse).any(|f| {
        f.len() >= 4
            && f[2] == "connected"
            && f[1] != "loopback"
            && f[1] != "tun"
            && f[3] != HOTSPOT_CONNECTION
    }))
}

pub async fn scan(iface: &str) -> Result<Vec<AccessPoint>> {
    let out = nmcli(&[
        "-t",
        "-f",
        "SSID,SIGNAL,SECURITY",
        "dev",
        "wifi",
        "list",
        "ifname",
        iface,
        "--rescan",
        "yes",
    ])
    .await?;
    let mut aps: Vec<AccessPoint> = Vec::new();
    for line in out.lines() {
        let f = split_terse(line);
        if f.len() < 3 || f[0].is_empty() {
            continue;
        }
        let ap = AccessPoint {
            ssid: f[0].clone(),
            signal: f[1].parse().unwrap_or(0),
            security: f[2].clone(),
        };
        match aps.iter_mut().find(|a| a.ssid == ap.ssid) {
            Some(existing) if existing.signal < ap.signal => *existing = ap,
            Some(_) => {}
            None => aps.push(ap),
        }
    }
    aps.sort_by_key(|a| std::cmp::Reverse(a.signal));
    Ok(aps)
}

/// Saved Wi-Fi profiles, excluding the setup hotspot.
pub async fn saved_networks() -> Result<Vec<String>> {
    let out = nmcli(&["-t", "-f", "NAME,TYPE", "con", "show"]).await?;
    Ok(out
        .lines()
        .map(split_terse)
        .filter(|f| f.len() >= 2 && f[1] == "802-11-wireless" && f[0] != HOTSPOT_CONNECTION)
        .map(|f| f[0].clone())
        .collect())
}

pub async fn forget(name: &str) -> Result<()> {
    nmcli(&["con", "delete", "id", name]).await.map(|_| ())
}

/// Adds (replacing any profile of the same name) and activates a Wi-Fi network.
pub async fn connect(iface: &str, ssid: &str, psk: &str) -> Result<()> {
    let _ = nmcli(&["con", "delete", "id", ssid]).await;
    let mut args = vec![
        "con",
        "add",
        "type",
        "wifi",
        "ifname",
        iface,
        "con-name",
        ssid,
        "ssid",
        ssid,
        "connection.autoconnect",
        "yes",
        "wifi.powersave",
        "2",
    ];
    if !psk.is_empty() {
        args.extend(["wifi-sec.key-mgmt", "wpa-psk", "wifi-sec.psk", psk]);
    }
    nmcli(&args).await?;
    nmcli(&["con", "up", "id", ssid]).await.map(|_| ())
}

/// Brings up the setup access point (WPA2 when `psk` is non-empty, open
/// otherwise) with a fixed address and shared DHCP/DNS.
///
/// WPA2/CCMP only and an explicit channel: Broadcom's brcmfmac (Pi Zero W)
/// refuses AP mode with NetworkManager's default WPA+WPA2/TKIP+CCMP mix and
/// fails with "Failed to initialize AP interface".
pub async fn hotspot_up(iface: &str, ssid: &str, psk: &str) -> Result<()> {
    let _ = nmcli(&["con", "delete", "id", HOTSPOT_CONNECTION]).await;
    let addr = format!("{HOTSPOT_ADDRESS}/24");
    let mut args = vec![
        "con",
        "add",
        "type",
        "wifi",
        "ifname",
        iface,
        "con-name",
        HOTSPOT_CONNECTION,
        "autoconnect",
        "no",
        "ssid",
        ssid,
        "mode",
        "ap",
        "wifi.band",
        "bg",
        "wifi.channel",
        "6",
        "wifi.powersave",
        "2",
        "ipv4.method",
        "shared",
        "ipv4.addresses",
        &addr,
        "ipv6.method",
        "disabled",
    ];
    if !psk.is_empty() {
        args.extend([
            "wifi-sec.key-mgmt",
            "wpa-psk",
            "wifi-sec.proto",
            "rsn",
            "wifi-sec.pairwise",
            "ccmp",
            "wifi-sec.group",
            "ccmp",
            "wifi-sec.psk",
            psk,
        ]);
    }
    nmcli(&args).await?;
    nmcli(&["con", "up", "id", HOTSPOT_CONNECTION])
        .await
        .map(|_| ())
}

pub async fn hotspot_down() -> Result<()> {
    let _ = nmcli(&["con", "down", "id", HOTSPOT_CONNECTION]).await;
    Ok(())
}

/// Pi OS keeps Wi-Fi rfkill-blocked until a regulatory domain is set. We may be
/// the only thing that ever sets one, so do it ourselves, once. Best effort.
pub async fn set_country(country: Option<&str>) {
    if let Some(cc) = country.filter(|c| !c.is_empty()) {
        // raspi-config knows the Pi-specific places to record the country.
        let rc = Command::new("raspi-config")
            .args(["nonint", "do_wifi_country", cc])
            .output()
            .await;
        if rc.as_ref().map(|o| o.status.success()).unwrap_or(false) {
            tracing::info!(country = cc, "wifi country set via raspi-config");
        } else {
            let _ = Command::new("iw").args(["reg", "set", cc]).output().await;
        }
    }
    radio_on().await;
}

/// Lifts the soft rfkill block and NetworkManager's own wireless switch.
pub async fn radio_on() {
    let _ = Command::new("rfkill")
        .args(["unblock", "wifi"])
        .output()
        .await;
    let _ = nmcli(&["radio", "wifi", "on"]).await;
}

/// Hands the device under test an address on the USB Ethernet gadget link
/// (and NAT to the internet through Wi-Fi). Idempotent.
pub async fn ensure_usb_ethernet() {
    let existing = nmcli(&["-t", "-f", "NAME", "con", "show"])
        .await
        .unwrap_or_default();
    if existing.lines().any(|l| l == "tailvision-usb") {
        return;
    }
    match nmcli(&[
        "con",
        "add",
        "type",
        "ethernet",
        "ifname",
        "usb0",
        "con-name",
        "tailvision-usb",
        "connection.autoconnect",
        "yes",
        "ipv4.method",
        "shared",
        "ipv4.addresses",
        "10.42.1.1/24",
        "ipv6.method",
        "disabled",
    ])
    .await
    {
        Ok(_) => tracing::info!("USB Ethernet gadget link configured (10.42.1.1/24)"),
        Err(e) => tracing::warn!(error = %e, "USB Ethernet gadget link"),
    }
}

pub async fn hostname() -> Result<String> {
    let out = Command::new("hostname")
        .output()
        .await
        .context("run hostname")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn set_hostname(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !valid {
        return Err(anyhow!("hostname must be letters, digits and hyphens"));
    }
    let out = Command::new("hostnamectl")
        .args(["set-hostname", name])
        .output()
        .await
        .context("run hostnamectl")?;
    if !out.status.success() {
        return Err(anyhow!(
            "hostnamectl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terse_split_handles_escaped_colons() {
        assert_eq!(
            split_terse("wlan0:wifi:connected:Home\\:Net"),
            vec!["wlan0", "wifi", "connected", "Home:Net"]
        );
        assert_eq!(
            split_terse("IP4.ADDRESS[1]:10.0.0.5/24"),
            vec!["IP4.ADDRESS[1]", "10.0.0.5/24"]
        );
        assert_eq!(split_terse(""), vec![""]);
    }
}
