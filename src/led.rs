//! Status on the board's activity LED (`/sys/class/leds/ACT` on a Pi Zero W).
//!
//! Blinking uses the kernel's `timer` trigger, so the pattern keeps going
//! without any help from us.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    On,
    /// On for `on_ms`, off for `off_ms`, repeated.
    Blink {
        on_ms: u32,
        off_ms: u32,
    },
}

impl Pattern {
    /// Short blip every two seconds: looking for a network.
    pub const SEARCHING: Pattern = Pattern::Blink {
        on_ms: 100,
        off_ms: 1900,
    };
    /// Fast blink: the setup hotspot is up, waiting for someone to configure us.
    pub const HOTSPOT: Pattern = Pattern::Blink {
        on_ms: 200,
        off_ms: 200,
    };
    /// Slow blink: on a network, but Tailscale is not logged in.
    pub const NO_TAILSCALE: Pattern = Pattern::Blink {
        on_ms: 1000,
        off_ms: 1000,
    };
    /// Solid: network and Tailscale are both up.
    pub const READY: Pattern = Pattern::On;
}

pub struct Led {
    dir: PathBuf,
    current: Option<Pattern>,
    /// Trigger that was active before we took over (`actpwr` on a Pi Zero,
    /// `mmc0` on most other boards), restored on exit.
    original_trigger: String,
}

/// The `trigger` file lists every trigger with the active one in brackets:
/// `none rc-feedback [actpwr] mmc0 timer ...`.
fn active_trigger(list: &str) -> Option<String> {
    let start = list.find('[')? + 1;
    let end = list[start..].find(']')? + start;
    Some(list[start..end].to_string())
}

impl Led {
    /// Picks the first LED directory that exists. Pi OS names the activity LED
    /// `ACT`; older kernels used `led0`.
    pub fn find(preferred: Option<&Path>) -> Option<Self> {
        let candidates = preferred.map(Path::to_path_buf).into_iter().chain(
            ["/sys/class/leds/ACT", "/sys/class/leds/led0"]
                .iter()
                .map(PathBuf::from),
        );
        candidates
            .filter(|d| d.join("trigger").exists())
            .map(|dir| {
                let original_trigger = std::fs::read_to_string(dir.join("trigger"))
                    .ok()
                    .and_then(|l| active_trigger(&l))
                    .unwrap_or_else(|| "none".into());
                Led {
                    dir,
                    current: None,
                    original_trigger,
                }
            })
            .next()
    }

    pub fn set(&mut self, pattern: Pattern) {
        if self.current == Some(pattern) {
            return;
        }
        match self.apply(pattern) {
            Ok(()) => self.current = Some(pattern),
            Err(e) => {
                tracing::warn!(error = %e, led = %self.dir.display(), ?pattern, "led update failed")
            }
        }
    }

    fn write(&self, file: &str, value: &str) -> std::io::Result<()> {
        std::fs::write(self.dir.join(file), value)
    }

    fn apply(&self, pattern: Pattern) -> std::io::Result<()> {
        match pattern {
            Pattern::On => {
                self.write("trigger", "none")?;
                self.write("brightness", "1")
            }
            Pattern::Blink { on_ms, off_ms } => {
                self.write("trigger", "timer")?;
                self.write("delay_on", &on_ms.to_string())?;
                self.write("delay_off", &off_ms.to_string())
            }
        }
    }
}

impl Drop for Led {
    fn drop(&mut self) {
        // Hand the LED back to whatever drove it before.
        let _ = self.write("trigger", &self.original_trigger);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_trigger_files() {
        let dir = std::env::temp_dir().join(format!("tailvision-led-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["brightness", "delay_on", "delay_off"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::write(dir.join("trigger"), "none [actpwr] mmc0 timer").unwrap();
        assert_eq!(
            active_trigger("none [actpwr] mmc0 timer").as_deref(),
            Some("actpwr")
        );
        assert_eq!(active_trigger("none mmc0"), None);
        let mut led = Led::find(Some(&dir)).expect("fake led found");
        led.set(Pattern::HOTSPOT);
        assert_eq!(
            std::fs::read_to_string(dir.join("trigger")).unwrap(),
            "timer"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("delay_on")).unwrap(),
            "200"
        );
        led.set(Pattern::READY);
        assert_eq!(
            std::fs::read_to_string(dir.join("trigger")).unwrap(),
            "none"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("brightness")).unwrap(),
            "1"
        );
        drop(led);
        assert_eq!(
            std::fs::read_to_string(dir.join("trigger")).unwrap(),
            "actpwr"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(Led::find(Some(&dir)).is_none() || Path::new("/sys/class/leds/ACT").exists());
    }
}
