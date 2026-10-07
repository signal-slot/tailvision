//! USB gadget: the unit plugs into the device under test and shows up as a
//! touch screen, a keyboard and a USB Ethernet adapter (configfs/libcomposite).
//!
//! Needs `dtoverlay=dwc2,dr_mode=peripheral` and the `dwc2` + `libcomposite`
//! modules; without a UDC present everything here is a no-op.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

const CONFIGFS: &str = "/sys/kernel/config/usb_gadget";
const GADGET: &str = "tailvision";

/// Digitizer logical range on both axes.
pub const TOUCH_MAX: u16 = 32767;

/// Single-touch digitizer: tip switch, in-range, 6 bits padding, X, Y (16-bit each).
const TOUCH_REPORT_DESC: &[u8] = &[
    0x05, 0x0D, // Usage Page (Digitizers)
    0x09, 0x04, // Usage (Touch Screen)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x22, //   Usage (Finger)
    0xA1, 0x02, //   Collection (Logical)
    0x09, 0x42, //     Usage (Tip Switch)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x09, 0x32, //     Usage (In Range)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x95, 0x06, //     Report Count (6)
    0x81, 0x03, //     Input (Const) padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x26, 0xFF, 0x7F, // Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x09, 0x31, //     Usage (Y)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0xC0, //         End Collection
    0xC0, //       End Collection
];
const TOUCH_REPORT_LEN: usize = 5;

/// Boot-protocol keyboard: modifiers, reserved, six key codes.
const KEYBOARD_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x03, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];
const KEYBOARD_REPORT_LEN: usize = 8;

pub struct Gadget {
    touch: PathBuf,
    keyboard: PathBuf,
    /// Serialises reports so a swipe is not interleaved with a tap.
    lock: Mutex<()>,
}

fn write(path: impl AsRef<Path>, value: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path.as_ref(), value.as_ref())
        .with_context(|| format!("write {}", path.as_ref().display()))
}

/// First UDC the kernel offers, or None when the port is in host mode.
fn udc_name() -> Option<String> {
    fs::read_dir("/sys/class/udc")
        .ok()?
        .flatten()
        .next()
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

/// Stable, locally administered MAC addresses derived from the machine id.
fn mac_pair() -> (String, String) {
    let id = fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.trim().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let b = h.to_le_bytes();
    let dev = format!("02:70:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3]);
    let host = format!(
        "02:70:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0],
        b[1],
        b[2],
        b[3] ^ 1
    );
    (dev, host)
}

/// Finds the /dev/hidgN node whose device number matches the function's `dev` file.
fn hidg_node(function_dir: &Path) -> Result<PathBuf> {
    let dev = fs::read_to_string(function_dir.join("dev"))
        .with_context(|| format!("read {}/dev", function_dir.display()))?;
    let (major, minor) = dev
        .trim()
        .split_once(':')
        .ok_or_else(|| anyhow!("odd dev file: {dev}"))?;
    let (major, minor): (u64, u64) = (major.parse()?, minor.parse()?);
    for _ in 0..20 {
        for entry in fs::read_dir("/dev")?.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("hidg") {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.file_type().is_char_device() {
                let rdev = meta.rdev();
                // Linux dev_t encoding (same as libc::major/minor).
                let (ma, mi) = (
                    ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff),
                    (rdev & 0xff) | ((rdev >> 12) & !0xff),
                );
                if ma == major && mi == minor {
                    return Ok(entry.path());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("no /dev/hidg* node for {}", function_dir.display())
}

impl Gadget {
    /// Builds and binds the gadget, or returns Ok(None) when there is no UDC.
    pub fn setup(product: &str, serial: &str) -> Result<Option<Gadget>> {
        let Some(udc) = udc_name() else {
            return Ok(None);
        };
        let g = PathBuf::from(CONFIGFS).join(GADGET);
        if !Path::new(CONFIGFS).exists() {
            bail!("{CONFIGFS} missing: is libcomposite loaded?");
        }
        let fresh = !g.exists();
        if fresh {
            fs::create_dir_all(&g)?;
            write(g.join("idVendor"), "0x1d6b")?; // Linux Foundation
            write(g.join("idProduct"), "0x0104")?; // Multifunction Composite Gadget
            write(g.join("bcdDevice"), "0x0100")?;
            write(g.join("bcdUSB"), "0x0200")?;
            fs::create_dir_all(g.join("strings/0x409"))?;
            write(g.join("strings/0x409/serialnumber"), serial)?;
            write(g.join("strings/0x409/manufacturer"), "Signal Slot Inc.")?;
            write(g.join("strings/0x409/product"), product)?;

            fs::create_dir_all(g.join("configs/c.1/strings/0x409"))?;
            write(
                g.join("configs/c.1/strings/0x409/configuration"),
                "touch + keyboard + ethernet",
            )?;
            write(g.join("configs/c.1/MaxPower"), "500")?;

            let touch = g.join("functions/hid.touch");
            fs::create_dir_all(&touch)?;
            write(touch.join("protocol"), "0")?;
            write(touch.join("subclass"), "0")?;
            write(touch.join("report_length"), TOUCH_REPORT_LEN.to_string())?;
            write(touch.join("report_desc"), TOUCH_REPORT_DESC)?;

            let kbd = g.join("functions/hid.keyboard");
            fs::create_dir_all(&kbd)?;
            write(kbd.join("protocol"), "1")?;
            write(kbd.join("subclass"), "1")?;
            write(kbd.join("report_length"), KEYBOARD_REPORT_LEN.to_string())?;
            write(kbd.join("report_desc"), KEYBOARD_REPORT_DESC)?;

            let ecm = g.join("functions/ecm.usb0");
            fs::create_dir_all(&ecm)?;
            let (dev_addr, host_addr) = mac_pair();
            write(ecm.join("dev_addr"), &dev_addr)?;
            write(ecm.join("host_addr"), &host_addr)?;

            for f in ["hid.touch", "hid.keyboard", "ecm.usb0"] {
                std::os::unix::fs::symlink(
                    g.join("functions").join(f),
                    g.join("configs/c.1").join(f),
                )?;
            }
        }
        let bound = fs::read_to_string(g.join("UDC"))
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !bound {
            write(g.join("UDC"), &udc).context("bind gadget to the UDC")?;
        }
        let touch = hidg_node(&g.join("functions/hid.touch"))?;
        let keyboard = hidg_node(&g.join("functions/hid.keyboard"))?;
        tracing::info!(udc, touch = %touch.display(), keyboard = %keyboard.display(), "USB gadget bound");
        Ok(Some(Gadget {
            touch,
            keyboard,
            lock: Mutex::new(()),
        }))
    }

    fn send(path: &Path, report: &[u8]) -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        f.write_all(report).with_context(|| {
            format!(
                "write {} (is the USB cable plugged into a host?)",
                path.display()
            )
        })
    }

    fn touch_report(&self, down: bool, x: u16, y: u16) -> Result<()> {
        let flags = if down { 0b11 } else { 0b00 };
        let [xl, xh] = x.min(TOUCH_MAX).to_le_bytes();
        let [yl, yh] = y.min(TOUCH_MAX).to_le_bytes();
        Self::send(&self.touch, &[flags, xl, xh, yl, yh])
    }

    /// Press at a point for `hold`, then release.
    pub fn tap(&self, x: u16, y: u16, hold: Duration) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        self.touch_report(true, x, y)?;
        std::thread::sleep(hold);
        self.touch_report(false, x, y)
    }

    /// Press, move in small steps over `duration`, release.
    pub fn swipe(&self, from: (u16, u16), to: (u16, u16), duration: Duration) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let steps = (duration.as_millis() / 16).clamp(4, 120) as u32;
        self.touch_report(true, from.0, from.1)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = from.0 as f32 + (to.0 as f32 - from.0 as f32) * t;
            let y = from.1 as f32 + (to.1 as f32 - from.1 as f32) * t;
            std::thread::sleep(duration / steps);
            self.touch_report(true, x.round() as u16, y.round() as u16)?;
        }
        std::thread::sleep(Duration::from_millis(30));
        self.touch_report(false, to.0, to.1)
    }

    fn key_report(&self, modifiers: u8, keys: &[u8]) -> Result<()> {
        let mut r = [0u8; 8];
        r[0] = modifiers;
        for (i, k) in keys.iter().take(6).enumerate() {
            r[2 + i] = *k;
        }
        Self::send(&self.keyboard, &r)
    }

    /// Presses and releases one key code with modifiers.
    pub fn key(&self, modifiers: u8, code: u8) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        self.key_report(modifiers, &[code])?;
        std::thread::sleep(Duration::from_millis(20));
        self.key_report(0, &[])
    }

    /// Types printable ASCII (US layout) plus newline and tab.
    pub fn type_text(&self, text: &str) -> Result<usize> {
        let _g = self.lock.lock().unwrap();
        let mut typed = 0;
        for ch in text.chars() {
            let Some((modifiers, code)) = ascii_to_usage(ch) else {
                continue;
            };
            self.key_report(modifiers, &[code])?;
            std::thread::sleep(Duration::from_millis(12));
            self.key_report(0, &[])?;
            std::thread::sleep(Duration::from_millis(12));
            typed += 1;
        }
        Ok(typed)
    }
}

pub const MOD_CTRL: u8 = 0x01;
pub const MOD_SHIFT: u8 = 0x02;
pub const MOD_ALT: u8 = 0x04;
pub const MOD_GUI: u8 = 0x08;

/// US-layout mapping of a character to (modifiers, HID usage).
pub fn ascii_to_usage(ch: char) -> Option<(u8, u8)> {
    let shifted = |c: u8| Some((MOD_SHIFT, c));
    let plain = |c: u8| Some((0, c));
    match ch {
        'a'..='z' => plain(0x04 + (ch as u8 - b'a')),
        'A'..='Z' => shifted(0x04 + (ch as u8 - b'A')),
        '1'..='9' => plain(0x1E + (ch as u8 - b'1')),
        '0' => plain(0x27),
        '\n' => plain(0x28),
        '\t' => plain(0x2B),
        ' ' => plain(0x2C),
        '-' => plain(0x2D),
        '_' => shifted(0x2D),
        '=' => plain(0x2E),
        '+' => shifted(0x2E),
        '[' => plain(0x2F),
        '{' => shifted(0x2F),
        ']' => plain(0x30),
        '}' => shifted(0x30),
        '\\' => plain(0x31),
        '|' => shifted(0x31),
        ';' => plain(0x33),
        ':' => shifted(0x33),
        '\'' => plain(0x34),
        '"' => shifted(0x34),
        '`' => plain(0x35),
        '~' => shifted(0x35),
        ',' => plain(0x36),
        '<' => shifted(0x36),
        '.' => plain(0x37),
        '>' => shifted(0x37),
        '/' => plain(0x38),
        '?' => shifted(0x38),
        '!' => shifted(0x1E),
        '@' => shifted(0x1F),
        '#' => shifted(0x20),
        '$' => shifted(0x21),
        '%' => shifted(0x22),
        '^' => shifted(0x23),
        '&' => shifted(0x24),
        '*' => shifted(0x25),
        '(' => shifted(0x26),
        ')' => shifted(0x27),
        _ => None,
    }
}

/// Named keys for `press_key`: "enter", "esc", "tab", "backspace", "space",
/// "up"/"down"/"left"/"right", "home", "end", "pageup", "pagedown",
/// "delete", "f1".."f12", or a single character.
pub fn key_name_to_usage(name: &str) -> Option<(u8, u8)> {
    let n = name.trim().to_ascii_lowercase();
    let code = match n.as_str() {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2A,
        "tab" => 0x2B,
        "space" => 0x2C,
        "capslock" => 0x39,
        "printscreen" => 0x46,
        "insert" => 0x49,
        "home" => 0x4A,
        "pageup" => 0x4B,
        "delete" | "del" => 0x4C,
        "end" => 0x4D,
        "pagedown" => 0x4E,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        _ => {
            if let Some(num) = n.strip_prefix('f').and_then(|f| f.parse::<u8>().ok())
                && (1..=12).contains(&num)
            {
                0x3A + (num - 1)
            } else {
                let mut chars = n.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                return ascii_to_usage(c);
            }
        }
    };
    Some((0, code))
}

/// Parses "ctrl+shift+c" style modifiers.
pub fn parse_modifiers(spec: &str) -> Result<u8> {
    let mut m = 0;
    for part in spec
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
    {
        m |= match part.as_str() {
            "ctrl" | "control" => MOD_CTRL,
            "shift" => MOD_SHIFT,
            "alt" | "option" => MOD_ALT,
            "gui" | "meta" | "super" | "win" | "cmd" => MOD_GUI,
            other => bail!("unknown modifier {other:?}"),
        };
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping() {
        assert_eq!(ascii_to_usage('a'), Some((0, 0x04)));
        assert_eq!(ascii_to_usage('Z'), Some((MOD_SHIFT, 0x1D)));
        assert_eq!(ascii_to_usage('0'), Some((0, 0x27)));
        assert_eq!(ascii_to_usage('!'), Some((MOD_SHIFT, 0x1E)));
        assert_eq!(ascii_to_usage('あ'), None);
        assert_eq!(key_name_to_usage("Enter"), Some((0, 0x28)));
        assert_eq!(key_name_to_usage("f12"), Some((0, 0x45)));
        assert_eq!(key_name_to_usage("f13"), None);
        assert_eq!(key_name_to_usage("x"), Some((0, 0x1B)));
        assert_eq!(parse_modifiers("ctrl+alt").unwrap(), MOD_CTRL | MOD_ALT);
        assert!(parse_modifiers("hyper").is_err());
    }

    #[test]
    fn descriptors_have_sane_sizes() {
        assert_eq!(TOUCH_REPORT_DESC.len(), 49);
        assert_eq!(KEYBOARD_REPORT_DESC.len(), 63);
        let (a, b) = mac_pair();
        assert_ne!(a, b);
        assert!(a.starts_with("02:70:"));
    }
}
