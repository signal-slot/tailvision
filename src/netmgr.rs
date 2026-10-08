//! NetworkManager control through `nmcli`: Wi-Fi status, scanning, joining a
//! network, and the open setup hotspot used when no network is reachable.

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

pub const HOTSPOT_CONNECTION: &str = "tailvision-hotspot";
/// NetworkManager connection for the USB Ethernet gadget toward the device under test.
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

/// Whether any interface (Wi-Fi client, Ethernet adapter, ...) other than the hotspot is up.
pub async fn any_online() -> Result<bool> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"]).await?;
    Ok(out.lines().map(split_terse).any(|f| {
        f.len() >= 4
            && f[2] == "connected"
            && f[1] != "loopback"
            && f[1] != "tun"
            && f[3] != HOTSPOT_CONNECTION
            // The debugging USB link toward the device under test is a shared
            // (downstream) network, not a way out.
            && f[3] != USB_CONNECTION
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
        // PMF must be off: wpa_supplicant defaults to "optional", which makes
        // its AP code install a management-frame key (IGTK) that the Zero W's
        // BCM43430 cannot do. The kernel then answers "key setting validation
        // failed" and the AP never starts.
        args.extend([
            "wifi-sec.key-mgmt",
            "wpa-psk",
            "wifi-sec.proto",
            "rsn",
            "wifi-sec.pairwise",
            "ccmp",
            "wifi-sec.group",
            "ccmp",
            "wifi-sec.pmf",
            "disable",
            "wifi-sec.psk",
            psk,
        ]);
    }
    nmcli(&args).await?;
    nmcli(&["con", "up", "id", HOTSPOT_CONNECTION]).await?;
    if !psk.is_empty() {
        pin_psk_only(iface).await;
    }
    Ok(())
}

/// Works around a mismatch that makes every client fail the WPA2 handshake
/// on the Zero W ("wrong password" on phones, reason 17 in wpa_supplicant):
/// NetworkManager 1.52 hands the AP `key_mgmt=WPA-PSK WPA-PSK-SHA256`, so
/// the handshake's RSN element lists both AKMs, but the BCM43430 firmware
/// builds the beacon's RSN element itself (the driver's attempt to install
/// hostapd's element fails with -52) and lists plain PSK only. Clients
/// compare the two and refuse. Restricting the running AP network to
/// WPA-PSK and restarting it makes both sides agree; NetworkManager keeps
/// the connection active across the restart. Done through wpa_supplicant's
/// D-Bus API (the control socket is not reliably there for NM's interface).
async fn pin_psk_only(iface: &str) {
    const BUS: &str = "fi.w1.wpa_supplicant1";
    let busctl = |args: Vec<String>| async move {
        Command::new("busctl")
            .args(&args)
            .output()
            .await
            .map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    String::from_utf8_lossy(&o.stderr).trim().to_string(),
                )
            })
            .unwrap_or((false, String::new(), "busctl missing".into()))
    };
    // `o "/fi/w1/..."` -> /fi/w1/...
    let path = |out: &str| out.split('"').nth(1).map(str::to_string);
    let s = |x: &str| x.to_string();
    let (_, out, err) = busctl(vec![
        s("call"),
        s(BUS),
        s("/fi/w1/wpa_supplicant1"),
        s(BUS),
        s("GetInterface"),
        s("s"),
        s(iface),
    ])
    .await;
    let Some(ifpath) = path(&out) else {
        tracing::warn!(
            err,
            "hotspot: wpa_supplicant interface not found; cannot pin key_mgmt"
        );
        return;
    };
    // The AP network appears a moment after activation.
    let mut net = None;
    for _ in 0..20 {
        let (_, out, _) = busctl(vec![
            s("get-property"),
            s(BUS),
            ifpath.clone(),
            format!("{BUS}.Interface"),
            s("CurrentNetwork"),
        ])
        .await;
        match path(&out) {
            Some(p) if p != "/" => {
                net = Some(p);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    let Some(net) = net else {
        tracing::warn!("hotspot: no current supplicant network; cannot pin key_mgmt");
        return;
    };
    let netif = format!("{BUS}.Network");
    let (ok1, _, e1) = busctl(vec![
        s("set-property"),
        s(BUS),
        net.clone(),
        netif.clone(),
        s("Properties"),
        s("a{sv}"),
        s("1"),
        s("key_mgmt"),
        s("s"),
        s("WPA-PSK"),
    ])
    .await;
    let (ok2, _, e2) = busctl(vec![
        s("set-property"),
        s(BUS),
        net.clone(),
        netif.clone(),
        s("Enabled"),
        s("b"),
        s("false"),
    ])
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let (ok3, _, e3) = busctl(vec![
        s("set-property"),
        s(BUS),
        net,
        netif,
        s("Enabled"),
        s("b"),
        s("true"),
    ])
    .await;
    if ok1 && ok2 && ok3 {
        tracing::info!("hotspot: key_mgmt pinned to WPA-PSK (brcmfmac beacon workaround)");
    } else {
        tracing::warn!(e1, e2, e3, "hotspot: pinning key_mgmt failed");
    }
}

/// Activates a saved network profile (named after its SSID) on the client interface.
pub async fn activate(ssid: &str) -> Result<()> {
    nmcli(&["--wait", "45", "con", "up", "id", ssid])
        .await
        .map(|_| ())
}

/// Can we reach the Internet (DNS and a TCP connection to Tailscale's login server)?
pub async fn internet_reachable() -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(6),
        tokio::net::TcpStream::connect(("login.tailscale.com", 443)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
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

/// NetworkManager connection for the debugging USB Ethernet link.
pub const USB_CONNECTION: &str = "tailvision-usb";

/// Hands the device under test an address on the USB Ethernet gadget link
/// (and NAT to the Internet through Wi-Fi). Debugging only: lets the unit
/// reach the target (SSH, ADB) over the cable. Idempotent.
pub async fn ensure_usb_ethernet() {
    let existing = nmcli(&["-t", "-f", "NAME", "con", "show"])
        .await
        .unwrap_or_default();
    if !existing.lines().any(|l| l == USB_CONNECTION) {
        match nmcli(&[
            "con",
            "add",
            "type",
            "ethernet",
            "ifname",
            "usb0",
            "con-name",
            USB_CONNECTION,
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
            Err(e) => {
                tracing::warn!(error = %e, "USB Ethernet gadget link");
                return;
            }
        }
    }
    // NetworkManager does not always autoconnect a connection added after the
    // gadget interface appeared, so bring it up explicitly; a failure here
    // (no host on the other end yet) is retried by autoconnect later.
    match nmcli(&["--wait", "20", "con", "up", "id", USB_CONNECTION]).await {
        Ok(_) => tracing::info!("USB Ethernet gadget link up"),
        Err(e) => tracing::info!(error = %e, "USB Ethernet gadget link not up yet"),
    }
}

pub async fn hostname() -> Result<String> {
    let out = Command::new("hostname")
        .output()
        .await
        .context("run hostname")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// dnsmasq options for the setup hotspot: `<hostname>`, `<hostname>.local`
/// and the OS connectivity-probe names (`web::CAPTIVE_PROBE_HOSTS`) resolve
/// to the unit's hotspot address, so `http://<hostname>/` works there as it
/// does on the tailnet, and the probes reach the setup page: a phone that
/// joins the hotspot opens its "sign in to network" window there. Every
/// other name goes to the real upstream DNS.
pub const LOCAL_DNS_CONF: &str = "/etc/NetworkManager/dnsmasq-shared.d/tailvision.conf";

pub fn write_local_dns(hostname: &str) {
    let mut text =
        String::from("# Written by tailvision on start and when the hostname changes.\n");
    let probes = crate::web::CAPTIVE_PROBE_HOSTS
        .iter()
        .map(|h| h.to_string());
    let own = [hostname.to_string(), format!("{hostname}.local")];
    for name in own.into_iter().chain(probes) {
        text.push_str(&format!("interface-name={name},wlan0\n"));
    }
    match std::fs::read_to_string(LOCAL_DNS_CONF) {
        Ok(cur) if cur == text => return,
        _ => {}
    }
    if let Err(e) = std::fs::write(LOCAL_DNS_CONF, text) {
        tracing::warn!(error = %e, path = LOCAL_DNS_CONF, "local DNS config");
    }
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
    write_local_dns(name);
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
