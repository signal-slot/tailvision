# tailvision

Eyes and a finger for AI agents on a device's physical screen.

> **Status (2026-10-07):** work in progress. Builds, unit tests and the SD
> image pipeline work; nothing has been verified on real hardware yet
> (camera capture, screen detection on real frames, USB gadget, hotspot).

A Raspberry Pi Zero W with a camera module points at the LCD of a board under
development, and plugs into that board's USB port as a touch screen, a
keyboard and a network adapter. An AI agent (Claude Code, or anything that
speaks MCP) calls `take_screenshot` and sees what is really on the panel:
rendering glitches, colours, fonts, the state right after boot, things a
host-side screenshot cannot show. Then it calls `tap` and presses what it saw.
Think of a NanoKVM for devices that have no HDMI output: the video comes
through a camera, the input through USB.

[日本語版 README](README.ja.md)

```
AI agent ──(Tailscale / HTTP)──▶ Pi Zero W: tailvision ──(CSI)──▶ camera module ──▶ target's LCD
                                       │     └──(USB, one cable)──▶ target: touch + keyboard + Ethernet + power
phone / PC ──(Wi-Fi or setup hotspot)──▶ setup page http://tailvision.local/
```

## What you get

- One static Rust binary. No Python, no OpenCV, no C libraries.
- MCP over Streamable HTTP at `/mcp`, with a bearer token that exists from the
  first boot. The camera is never open to the network.
- A setup page at `/` for Wi-Fi, hostname, Tailscale and the access key.
  When the unit cannot find a network it raises its own WPA2 hotspot so the
  page can be reached from a phone.
- Tailscale built in, so the unit is reachable by name from anywhere.
- The board's green LED shows the state at a glance.

## MCP tools

| Tool              | Returns | Notes                                                                 |
| ----------------- | ------- | --------------------------------------------------------------------- |
| `take_screenshot` | image   | The LCD only, found in the frame and perspective-corrected, as JPEG   |
| `capture_debug`   | image   | The raw frame with the detected outline drawn on (big dot = top-left) |
| `tap`             | text    | Touch a point, in pixels of the last `take_screenshot` image          |
| `swipe`           | text    | Drag between two points of the last screenshot                        |
| `type_text`       | text    | Type ASCII text on the USB keyboard                                   |
| `calibrate_camera`| image   | Measure focus/exposure/white balance once and lock them (`lock=false` to undo) |
| `press_key`       | text    | One key with modifiers, e.g. `enter`, `ctrl`+`c`, `f5`                |
| `list_formats`    | text    | Pixel formats and frame sizes a UVC camera offers                     |

`take_screenshot` finds the screen on every call (the camera need not be
fixed). Parameters: `raw=true` returns the whole camera frame;
`reuse_detection=true` keeps the previous corners (faster on a fixed rig);
`manual_corners=[[x,y],...]` overrides detection; `output_width`/`output_height`,
`margin_ratio`, `rotation_degrees` (multiples of 90), `flip_horizontal`,
`flip_vertical`, `min_area_ratio`; and the capture controls `width`/`height`,
`skip_frames`, `quality`.

Detection looks for the LCD-likeliest convex quadrilateral: candidates from
straight edges, bright/dark masks and the extent of bright UI content, scored
by aspect ratio, size, how much of the content they contain and whether a real
brightness step runs along their border. A dark panel on a dark surface with
no visible bezel is the hard case; there the result is the extent of what is
drawn on the screen. Check with `capture_debug` and fall back to
`manual_corners` when it gets it wrong.

Touch coordinates are pixels of the image the agent just received, so the
agent can look, decide and tap without knowing anything about the camera. The
unit maps them through the detected corners onto the device's touch screen
(`frame_coords=true` accepts raw-frame pixels instead, as seen in
`capture_debug`). `tap` with `hold_ms=800` is a long press.

Two camera stacks: a CSI camera module through libcamera (`rpicam-still`,
exposure/white balance/autofocus settle for `--csi-settle-ms`, default 1.5 s),
or a UVC webcam through V4L2 (MJPEG passed through with the Huffman tables
inserted, YUYV encoded on the Pi, the first `skip_frames` frames dropped).
`--camera auto` picks the CSI module when libcamera sees one.

## HTTP endpoints

| Path         | Purpose                                                        |
| ------------ | -------------------------------------------------------------- |
| `/`          | Setup page                                                     |
| `/screen.jpg` | The detected screen, `?ow=800&reuse=1`, plus the capture controls below |
| `/debug.jpg`  | Raw frame with the detected outline drawn on                   |
| `/shot.jpg`  | Raw frame, `?w=1280&h=720&skip=10&q=85`, for curl and browsers |
| `/mcp`       | MCP (Streamable HTTP)                                          |
| anything else | redirects to `/` (so phones' captive-portal prompts land there) |

Port 80.

## First boot

1. Flash the image, plug in the camera and power, switch on. The very first
   boot of a Zero W takes about 7 minutes (partition resize, reboot, then a
   slow cloud-init run); later boots take about a minute.
2. If no known Wi-Fi is reachable 60 seconds after boot, the unit raises the
   Wi-Fi network **`tailvision-setup`** (WPA2, password **`tailvision-setup`**
   unless the unit shipped with its own, see below). The LED blinks fast.
3. Join it from a phone or laptop and open `http://tailvision.local/`. While the
   hotspot is up every name resolves to the unit, and phones usually offer
   their "sign in to network" prompt, which leads to the same page.
4. On the page:
   - **Wi-Fi**: pick a network, enter the password, Join. The hotspot drops and
     the unit joins that network; move your phone to the same network and
     reopen `http://tailvision.local/`. If the hotspot is back within two
     minutes the password was wrong.
   - **Hostname**: rename when you run several units. The `.local` name, the
     Tailscale name and the hotspot name all follow.
   - **Tailscale**: "Get a login link" shows a URL to approve the node from any
     device. An auth key works too.
   - **Camera** (CSI module only): "Calibrate and lock" runs autofocus,
     auto-exposure and auto white balance once on the lit screen and freezes
     the result. Every later screenshot uses the same settings, so images are
     consistent, there is no settling delay, and autofocus cannot hunt on a
     dark UI. "Back to automatic" undoes it. The same is available to the
     agent as `calibrate_camera`.
   - **Image**: default rotation (0/90/180/270) and mirroring of the returned
     screenshot, for a camera mounted sideways or upside down. Tool calls that
     pass their own `rotation_degrees`/flips override it.
   - **Access key**: shown on the page together with the exact `claude mcp add`
     command. "Generate a new key" rotates it.
5. Carried out of Wi-Fi range, the unit raises the hotspot again after 90
   seconds and retries the saved networks every 10 minutes.

### LED (the green ACT LED on the board)

| LED                      | State                                        |
| ------------------------ | -------------------------------------------- |
| short blip every 2 s     | looking for a network                        |
| fast blink (0.2 s)       | setup hotspot is up                          |
| slow blink (1 s)         | on Wi-Fi, Tailscale not logged in            |
| solid                    | Wi-Fi and Tailscale both up: ready to use    |

### Who can do what

- `/mcp` and the `.jpg` endpoints always require `Authorization: Bearer <key>`
  (`?key=` also works for the images). A key is generated on the very first start.
- The setup page requires HTTP Basic auth (any user name, the key as password),
  except for clients connected through the setup hotspot: whoever has the
  hotspot's WPA2 password is treated as standing next to the unit, and that is
  how the key is read the first time.
- Units can ship with their own hotspot password: write it to
  `hotspot-password` on the boot (FAT) partition and print it on the label.

### When a unit never shows up

Put its card into any PC and read `tailvision-status.txt` on the boot (FAT)
partition: Wi-Fi state, hotspot, Tailscale state and the last error, rewritten
whenever something changes. The systemd journal on the root partition is
persistent too (`journalctl -D <mount>/var/log/journal -u tailvision`).

Files on the boot partition that the unit reads:

| File                 | Purpose                                                  |
| -------------------- | -------------------------------------------------------- |
| `wifi-country`       | ISO 3166-1 alpha-2 code; needed before Wi-Fi may transmit |
| `hotspot-password`   | per-unit WPA2 password for the setup hotspot             |
| `tailscale-authkey`  | joins the tailnet on boot, then the file is deleted      |

## Registering with Claude Code

Copy the command shown on the setup page:

```bash
claude mcp add --transport http tailvision http://tailvision/mcp \
  --header "Authorization: Bearer <key>"
```

or in `.mcp.json`:

```json
{
  "mcpServers": {
    "tailvision": {
      "type": "http",
      "url": "http://tailvision/mcp",
      "headers": { "Authorization": "Bearer <key>" }
    }
  }
}
```

`tailvision` resolves through Tailscale MagicDNS from anywhere, and
`tailvision.local` through mDNS on the same LAN.

## Hardware

- Raspberry Pi Zero W (or Zero 2 W). Pi OS Lite 32-bit, Trixie or later.
- Raspberry Pi Camera Module 3 (autofocus, good for 10–20 cm) or Module 2 /
  OV5647 (fixed focus), with the 22-pin Zero camera ribbon cable.
- A micro-B to USB-A cable from the port marked **USB** to the device under
  test. That one cable carries power, touch, keyboard and USB Ethernet.
  To keep the unit alive while the target is power-cycled, feed **PWR IN**
  from a separate supply and put a Schottky diode in the VBUS line of the
  target cable so the unit does not back-feed 5 V into it.
- An 8 GB or larger microSD card.
- A document-camera style arm or mount that keeps the camera over the panel.

With a UVC webcam instead: plug it into the **USB** port through an OTG
cable, power the unit from **PWR IN**, and delete the
`dtoverlay=dwc2,dr_mode=peripheral` line from `config.txt` (the port is then
a host and touch/keyboard are unavailable).

### What the device under test sees

A composite USB device: a single-touch digitizer (absolute, 0..32767 on
both axes, mapped by the target to its own display), a boot-protocol
keyboard, and a CDC ECM network adapter. The unit hands the target an
address on 10.42.1.0/24 and NATs it to its Wi-Fi, so the target can reach
the internet and the unit can reach the target (SSH, ADB) at the address the
target picked. Linux, Android, macOS and most RTOS USB stacks need no driver;
Windows needs an RNDIS variant (not built yet).

## Building

Cross-compiles from Linux x86_64 to a static ARMv6 musl binary using the
`rust-lld` shipped with the toolchain; no cross GCC needed.

```bash
rustup target add arm-unknown-linux-musleabihf
cargo build --release --target arm-unknown-linux-musleabihf
# -> target/arm-unknown-linux-musleabihf/release/tailvision
cargo test                                   # no camera needed
```

To run on the development machine:
`cargo run -- --bind 127.0.0.1:8080 --state-dir ./state --no-hotspot --no-led`.

## Making the SD card image

```bash
deploy/build-image.sh            # -> build/tailvision.img, about a minute
```

The script downloads the official Raspberry Pi OS Lite image (32-bit, Trixie)
and the Tailscale static binary into `build/`, loop-mounts the image (needs
sudo), installs the binary and the services, and writes cloud-init files for
the hostname and user. It asks for a Wi-Fi network (Enter to skip and rely on
the hotspot) and a Tailscale auth key (optional). Nothing else is baked in by
default, so the image can be handed to someone else; `deploy/image.env.example`
lists what can be added (your SSH key, a per-unit hotspot password, ...).

Flash with Raspberry Pi Imager ("Use custom", answer No to OS customisation) or:

```bash
sudo dd if=build/tailvision.img of=/dev/sdX bs=4M status=progress conv=fsync
```

The first boot takes two to three minutes.

To install on an existing Raspberry Pi OS instead:

```bash
deploy/install.sh pi@tailvision    # binary + systemd units; install Tailscale separately
```

If the card sits in a [microsd-ota](https://github.com/signal-slot/microsd-ota)
bridge, `deploy/update-via-bridge.sh` copies just the binary and units onto
the card over USB while the Pi is powered off, which is far quicker than
re-flashing a rebuilt image (every rebuild changes the ext4 layout, so even a
delta flash moves hundreds of megabytes).

## Options

```
tailvision [--bind 0.0.0.0:80] [--camera auto|csi|uvc] [--csi-settle-ms 1500]
           [--csi-args "--hflip"] [--no-gadget] [--device /dev/video0]
           [--width W --height H] [--skip-frames 10] [--quality 85]
           [--frame-timeout 5] [--state-dir /var/lib/tailvision]
           [--wifi-iface wlan0] [--hotspot-after 60] [--hotspot-retry 600]
           [--hotspot-password ...] [--hotspot-password-file /boot/firmware/hotspot-password]
           [--no-hotspot] [--led /sys/class/leds/ACT] [--no-led]
```

`RUST_LOG=debug` for more logging.

## Pointing a camera at an LCD

- Framing: every screenshot reports how much of the frame the screen fills.
  With the Camera Module 3 (66° horizontal) a 7-inch panel fills the frame at
  about 12 cm, a 5-inch one at about 9 cm; 30-70% fill is plenty, the
  detector crops the rest.
- Focus, exposure, white balance: calibrate once (see above) rather than
  fighting the automatics on every shot. A shutter time longer than the
  backlight's PWM period (10 ms or more) also removes the flicker bands.
- Moiré: tilt the camera a few degrees or change the capture size.
- Flicker from backlight PWM: lock the exposure, e.g.
  `v4l2-ctl -c exposure_auto=1 -c exposure_absolute=...`.
- Reflections: shade the panel and the camera from room lights.

## Limitations

- Colour is not calibrated: what the camera sees is what you get.
- Tailscale login needs a network with internet access.
- The hotspot password is fixed per unit and printed on it (or the default in
  this README), so it protects against passers-by, not against someone who
  holds the unit.

## License

MIT. Copyright (c) 2026 Signal Slot Inc.
