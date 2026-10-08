//! Tailscale control through the `tailscale` CLI.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct Status {
    /// `Running`, `NeedsLogin`, `Stopped`, `NoState`, or `unavailable` when the CLI fails.
    pub backend_state: String,
    pub dns_name: Option<String>,
    pub ips: Vec<String>,
    pub auth_url: Option<String>,
    pub error: Option<String>,
}

impl Status {
    pub fn is_running(&self) -> bool {
        self.backend_state == "Running"
    }
}

pub async fn status() -> Status {
    let out = match Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return Status {
                backend_state: "unavailable".into(),
                error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    if !out.status.success() {
        return Status {
            backend_state: "unavailable".into(),
            error: Some(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            ..Default::default()
        };
    }
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            return Status {
                backend_state: "unavailable".into(),
                error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    let s = |p: &serde_json::Value| p.as_str().map(|s| s.trim_end_matches('.').to_string());
    Status {
        backend_state: v["BackendState"].as_str().unwrap_or("unknown").to_string(),
        dns_name: s(&v["Self"]["DNSName"]).filter(|d| !d.is_empty()),
        ips: v["Self"]["TailscaleIPs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        auth_url: s(&v["AuthURL"]).filter(|u| !u.is_empty()),
        error: None,
    }
}

/// Joins the tailnet non-interactively with an auth key.
pub async fn up_with_key(key: &str) -> Result<()> {
    let out = Command::new("tailscale")
        .args([
            "up",
            "--reset",
            "--ssh",
            "--timeout",
            "90s",
            "--auth-key",
            key.trim(),
        ])
        .output()
        .await
        .context("run tailscale")?;
    if !out.status.success() {
        return Err(anyhow!(
            "tailscale up failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Renames the node; only meaningful once logged in.
pub async fn set_hostname(name: &str) -> Result<()> {
    let out = Command::new("tailscale")
        .args(["set", "--hostname", name])
        .output()
        .await
        .context("run tailscale")?;
    if !out.status.success() {
        return Err(anyhow!(
            "tailscale set failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// A browser login in progress: `tailscale up` keeps running until the user
/// finishes authenticating on the URL it printed.
#[derive(Default)]
pub struct Login {
    inner: Mutex<LoginState>,
}

/// Leaves the tailnet: the node key is dropped and the machine disappears
/// from the admin console, so the unit can be re-provisioned or handed on.
pub async fn logout() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["logout"])
        .output()
        .await
        .context("run tailscale")?;
    if !out.status.success() {
        return Err(anyhow!(
            "tailscale logout failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[derive(Default)]
struct LoginState {
    auth_url: Option<String>,
    running: bool,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LoginView {
    pub auth_url: Option<String>,
    pub running: bool,
    pub last_error: Option<String>,
}

impl Login {
    pub async fn view(&self) -> LoginView {
        let s = self.inner.lock().await;
        LoginView {
            auth_url: s.auth_url.clone(),
            running: s.running,
            last_error: s.last_error.clone(),
        }
    }

    /// Forgets a login link from before a logout.
    pub async fn reset(&self) {
        *self.inner.lock().await = LoginState::default();
    }

    /// Starts `tailscale up --json` in the background; the login URL it prints
    /// shows up in `view`. The process runs until the login completes: the
    /// unit is back on the hotspot (no Internet) while the user approves the
    /// link elsewhere, and only "Go online" lets tailscaled pick up the
    /// approval, so no timeout here, or the link dies before it can be used.
    pub async fn start(self: &Arc<Self>) -> Result<Option<String>> {
        {
            let mut s = self.inner.lock().await;
            if s.running {
                return Ok(s.auth_url.clone());
            }
            s.running = true;
            s.auth_url = None;
            s.last_error = None;
        }
        let mut child = Command::new("tailscale")
            .args(["up", "--reset", "--ssh", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn tailscale")?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                    && let Some(url) = v["AuthURL"].as_str()
                    && !url.is_empty()
                {
                    me.inner.lock().await.auth_url = Some(url.to_string());
                }
            }
            let mut err = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut BufReader::new(stderr), &mut err)
                .await;
            let status = child.wait().await;
            let mut s = me.inner.lock().await;
            s.running = false;
            s.auth_url = None;
            match status {
                Ok(st) if st.success() => {}
                Ok(_) => s.last_error = Some(err.trim().to_string()),
                Err(e) => s.last_error = Some(e.to_string()),
            }
        });
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let s = self.inner.lock().await;
            if s.auth_url.is_some() || !s.running {
                return Ok(s.auth_url.clone());
            }
        }
        Ok(None)
    }
}
