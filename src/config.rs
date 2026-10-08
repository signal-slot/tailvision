//! Persisted settings (currently just the access key) in the state directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// Bearer token for /mcp and /shot.jpg, and Basic-auth password for the setup page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_key: Option<String>,
    /// Fixed focus / exposure / white balance for the CSI camera, from the
    /// setup page's "Calibrate" or the calibrate_camera tool. None = automatic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_lock: Option<crate::libcamera::CameraLock>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("config.json"),
        }
    }

    pub fn load(&self) -> Result<Config> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(&serde_json::to_vec_pretty(config)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename to {}", self.path.display()))
    }
}

/// 48 hex characters from the kernel's RNG.
pub fn generate_key() -> Result<String> {
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .context("read /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("tailvision-test-{}", std::process::id()));
        let store = Store::new(&dir);
        assert!(store.load().unwrap().mcp_key.is_none());
        let key = generate_key().unwrap();
        assert_eq!(key.len(), 48);
        store
            .save(&Config {
                mcp_key: Some(key.clone()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.load().unwrap().mcp_key.as_deref(), Some(key.as_str()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
