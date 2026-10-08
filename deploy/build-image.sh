#!/bin/bash
# Builds a ready-to-flash SD card image for a Raspberry Pi Zero W:
# official Raspberry Pi OS Lite (32-bit, Trixie) + tailvision + Tailscale,
# with hostname / user / Wi-Fi / SSH pre-configured through cloud-init.
#
#   deploy/build-image.sh            # -> build/<HOSTNAME>.img (default tailvision.img)
#
# Defaults (hostname tailvision, user pi, your ~/.ssh/id_ed25519.pub, JP, Asia/Tokyo)
# can be overridden in deploy/image.env (see image.env.example); anything
# missing there is asked for interactively.
#
# Needs: sudo (loop-mounting the image), xz, curl, openssl, losetup, parted,
# e2fsck, resize2fs, and the ARMv6 build of tailvision (built if missing).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
build="$root/build"
env_file="${IMAGE_ENV:-$root/deploy/image.env}"

BASE_IMAGE_URL="${BASE_IMAGE_URL:-https://downloads.raspberrypi.com/raspios_lite_armhf/images/raspios_lite_armhf-2026-09-15/2026-09-15-raspios-trixie-armhf-lite.img.xz}"
TAILSCALE_VERSION="${TAILSCALE_VERSION:-1.102.4}"

# ---------------------------------------------------------------- settings
if [ -f "$env_file" ]; then
    # shellcheck disable=SC1090
    . "$env_file"
fi
HOSTNAME="${HOSTNAME:-tailvision}"
USERNAME="${USERNAME:-pi}"
WIFI_COUNTRY="${WIFI_COUNTRY:-JP}"

# Anything still missing is asked for on the terminal. Secrets are read
# without echo and never written to disk outside the image itself.
ask() { # ask VAR "prompt" [silent]
    local var="$1" prompt="$2" silent="${3:-}" value
    [ -n "${!var:-}" ] && return 0
    [ -t 0 ] || { echo "$var is not set and stdin is not a terminal; put it in $env_file" >&2; exit 1; }
    if [ -n "$silent" ]; then
        read -rsp "$prompt: " value; echo
    else
        read -rp "$prompt: " value
    fi
    printf -v "$var" '%s' "$value"
}
ask HOSTNAME "Hostname"
# Wi-Fi is optional: without it the Pi raises the "<hostname>-setup" hotspot on
# first boot and you configure Wi-Fi, Tailscale and the access key from
# http://<hostname>.local/ on your phone.
if [ -t 0 ] && [ -z "${WIFI_SSID:-}" ]; then
    read -rp "Wi-Fi SSID (Enter to skip and use the setup hotspot): " WIFI_SSID
fi
WIFI_SSID="${WIFI_SSID:-}"
WIFI_PSK="${WIFI_PSK:-}"
if [ -n "$WIFI_SSID" ] && [ -z "$WIFI_PSK" ] && [ -t 0 ]; then
    read -rsp "Wi-Fi password (Enter for an open network): " WIFI_PSK; echo
fi
if [ -t 0 ] && [ -z "${TAILSCALE_AUTHKEY:-}" ]; then
    read -rsp "Tailscale auth key (optional, Enter to skip): " TAILSCALE_AUTHKEY; echo
fi
TAILSCALE_AUTHKEY="${TAILSCALE_AUTHKEY:-}"
TIMEZONE="${TIMEZONE:-Asia/Tokyo}"
# No SSH key or password is baked in by default: the image may be handed to
# others. Shell access comes through Tailscale SSH once the unit is logged in.
SSH_PUBKEY_FILE="${SSH_PUBKEY_FILE:-}"
SSH_PUBKEY_FILE="${SSH_PUBKEY_FILE/#\$HOME/$HOME}"
PASSWORD="${PASSWORD:-}"
TAILSCALE_AUTHKEY="${TAILSCALE_AUTHKEY:-}"
HOTSPOT_PASSWORD="${HOTSPOT_PASSWORD:-}"

ssh_pubkey=""
if [ -n "$SSH_PUBKEY_FILE" ] && [ -f "$SSH_PUBKEY_FILE" ]; then
    ssh_pubkey="$(cat "$SSH_PUBKEY_FILE")"
fi

mkdir -p "$build"
out="$build/$HOSTNAME.img"

# ---------------------------------------------------------------- inputs
base_xz="$build/$(basename "$BASE_IMAGE_URL")"
base_img="${base_xz%.xz}"
if [ ! -f "$base_img" ]; then
    if [ ! -f "$base_xz" ]; then
        echo ">> downloading $(basename "$base_xz")"
        curl -L --progress-bar -o "$base_xz" "$BASE_IMAGE_URL"
        curl -sL -o "$base_xz.sha256" "$BASE_IMAGE_URL.sha256"
        (cd "$build" && sha256sum -c "$(basename "$base_xz").sha256")
    fi
    echo ">> extracting $(basename "$base_xz")"
    xz -dk "$base_xz"
fi

ts_tgz="$build/tailscale_${TAILSCALE_VERSION}_arm.tgz"
ts_dir="$build/tailscale_${TAILSCALE_VERSION}_arm"
if [ ! -x "$ts_dir/tailscaled" ]; then
    [ -f "$ts_tgz" ] || { echo ">> downloading tailscale $TAILSCALE_VERSION"; curl -L --progress-bar -o "$ts_tgz" "https://pkgs.tailscale.com/stable/$(basename "$ts_tgz")"; }
    tar xzf "$ts_tgz" -C "$build"
fi

bin="$root/target/arm-unknown-linux-musleabihf/release/tailvision"
if [ ! -x "$bin" ]; then
    echo ">> building tailvision for ARMv6"
    (cd "$root" && cargo build --release --target arm-unknown-linux-musleabihf)
fi

# ---------------------------------------------------------------- image
echo ">> copying base image to $out"
cp --reflink=auto "$base_img" "$out"

# Make sure the root filesystem has room for Tailscale (~60 MB) and friends.
# Raspberry Pi OS expands the root partition to the card on first boot anyway.
echo ">> growing root partition by 256 MB"
truncate -s +256M "$out"
parted -s "$out" resizepart 2 100%

loop="$(sudo losetup -Pf --show "$out")"
bootmnt="$(mktemp -d)"
rootmnt="$(mktemp -d)"
finished=0
cleanup() {
    set +e
    sudo umount "$bootmnt" 2>/dev/null
    sudo umount "$rootmnt" 2>/dev/null
    sudo losetup -d "$loop" 2>/dev/null
    rmdir "$bootmnt" "$rootmnt" 2>/dev/null
    if [ "$finished" = 0 ]; then
        echo "!! build failed, removing the half-built $out" >&2
        rm -f "$out"
    fi
}
trap cleanup EXIT

sudo e2fsck -fp "${loop}p2" >/dev/null
sudo resize2fs "${loop}p2" >/dev/null 2>&1
sudo mount "${loop}p1" "$bootmnt"
sudo mount "${loop}p2" "$rootmnt"

# ---------------------------------------------------------------- cloud-init (boot partition)
echo ">> writing cloud-init user-data / network-config"
passwd_lines=""
if [ -n "$PASSWORD" ]; then
    hash="$(openssl passwd -6 "$PASSWORD")"
    passwd_lines="    lock_passwd: false
    passwd: \"$hash\""
else
    passwd_lines="    lock_passwd: true"
fi
key_lines=""
if [ -n "$ssh_pubkey" ]; then
    key_lines="    ssh_authorized_keys:
      - \"$ssh_pubkey\""
fi

sudo tee "$bootmnt/user-data" >/dev/null <<EOF
#cloud-config

hostname: $HOSTNAME
manage_etc_hosts: true
timezone: $TIMEZONE

users:
  - name: $USERNAME
    groups: users,adm,dialout,audio,netdev,video,plugdev,input,gpio,spi,i2c,render,sudo
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
$passwd_lines
$key_lines

enable_ssh: $([ -n "$ssh_pubkey" ] && echo true || echo false)
ssh_pwauth: false

runcmd:
  # Belt and braces for the Wi-Fi regulatory domain / rfkill on first boot.
  - [ sh, -c, "raspi-config nonint do_wifi_country $WIFI_COUNTRY || true" ]
  - [ sh, -c, "rfkill unblock wifi || true" ]
EOF

{
    cat <<EOF
network:
  version: 2
  ethernets:
    eth0:
      dhcp4: true
      optional: true
EOF
    if [ -n "$WIFI_SSID" ]; then
        cat <<EOF
  wifis:
    renderer: NetworkManager
    wlan0:
      dhcp4: true
      regulatory-domain: "$WIFI_COUNTRY"
      access-points:
        "$WIFI_SSID":
EOF
        [ -n "$WIFI_PSK" ] && echo "          password: \"$WIFI_PSK\""
        echo "      optional: true"
    fi
} | sudo tee "$bootmnt/network-config" >/dev/null

if [ -n "$TAILSCALE_AUTHKEY" ]; then
    printf '%s\n' "$TAILSCALE_AUTHKEY" | sudo tee "$bootmnt/tailscale-authkey" >/dev/null
fi
if [ -n "$HOTSPOT_PASSWORD" ]; then
    # Per-unit hotspot password (print it on the label). Without this file the
    # binary's default applies.
    printf '%s\n' "$HOTSPOT_PASSWORD" | sudo tee "$bootmnt/hotspot-password" >/dev/null
fi
# sshd listens on LAN and the USB link only when the builder supplied a key
# (development units). Everything else uses Tailscale SSH, which is gated by
# the tailnet's ACL, or the setup page. Raspberry Pi OS enables sshd when
# this marker file exists, so it is created only in that case.
if [ -n "$ssh_pubkey" ]; then
    sudo touch "$bootmnt/ssh"
else
    sudo rm -f "$bootmnt/ssh"
fi
# USB port in peripheral (gadget) mode: touch screen + keyboard + Ethernet for
# the device under test, which also powers the unit. Comment this out to use a
# USB webcam on the port instead.
if ! grep -q 'dr_mode=peripheral' "$bootmnt/config.txt"; then
    printf '\n# tailvision: USB gadget mode (touch, keyboard, Ethernet to the device under test)\n[all]\ndtoverlay=dwc2,dr_mode=peripheral\n' | sudo tee -a "$bootmnt/config.txt" >/dev/null
fi
# Wi-Fi regulatory domain: the kernel parameter is what raspi-config sets, and
# the file lets tailvision re-apply it and lift the rfkill block itself.
printf '%s\n' "$WIFI_COUNTRY" | sudo tee "$bootmnt/wifi-country" >/dev/null
sudo sed -i -e "s/\s*cfg80211.ieee80211_regdom=\S*//" -e "s/\$/ cfg80211.ieee80211_regdom=$WIFI_COUNTRY/" "$bootmnt/cmdline.txt"

# ---------------------------------------------------------------- root filesystem
echo ">> installing tailvision and tailscale into the root filesystem"
sudo install -m 755 "$bin" "$rootmnt/usr/local/bin/tailvision"
sudo install -m 644 "$root/deploy/tailvision.service" "$rootmnt/etc/systemd/system/tailvision.service"

sudo install -m 755 "$ts_dir/tailscaled" "$rootmnt/usr/sbin/tailscaled"
sudo install -m 755 "$ts_dir/tailscale" "$rootmnt/usr/bin/tailscale"
sudo install -m 644 "$ts_dir/systemd/tailscaled.service" "$rootmnt/etc/systemd/system/tailscaled.service"
sudo install -m 644 "$ts_dir/systemd/tailscaled.defaults" "$rootmnt/etc/default/tailscaled"
sudo install -m 644 "$root/deploy/tailscale-firstboot.service" "$rootmnt/etc/systemd/system/tailscale-firstboot.service"
sudo install -m 644 "$root/deploy/modules-gadget.conf" "$rootmnt/etc/modules-load.d/tailvision-gadget.conf"
sudo install -m 644 "$root/deploy/modprobe-gadget.conf" "$rootmnt/etc/modprobe.d/tailvision-gadget.conf"
sudo install -d "$rootmnt/etc/systemd/journald.conf.d"
sudo install -m 644 "$root/deploy/journald-persistent.conf" "$rootmnt/etc/systemd/journald.conf.d/tailvision.conf"
sudo install -d "$rootmnt/etc/NetworkManager/dnsmasq-shared.d"
sudo install -m 644 "$root/deploy/dnsmasq-hotspot.conf" "$rootmnt/etc/NetworkManager/dnsmasq-shared.d/tailvision.conf"

# Equivalent of `systemctl enable` without booting the image.
wants="$rootmnt/etc/systemd/system/multi-user.target.wants"
sudo mkdir -p "$wants"
for unit in tailvision.service tailscaled.service tailscale-firstboot.service; do
    sudo ln -sf "/etc/systemd/system/$unit" "$wants/$unit"
done

sync
finished=1
cleanup
trap - EXIT

echo
echo "image ready: $out"
echo "flash it with Raspberry Pi Imager (choose 'Use custom', skip OS customisation) or:"
echo "  sudo dd if=$out of=/dev/sdX bs=4M status=progress conv=fsync"
if [ -z "$WIFI_SSID" ]; then
    echo "no Wi-Fi configured: about two minutes after power-on, join the Wi-Fi \"$HOSTNAME-setup\" (password: ${HOTSPOT_PASSWORD:-tailvision-setup}) and open http://$HOSTNAME.local/"
elif [ -z "$TAILSCALE_AUTHKEY" ]; then
    echo "no Tailscale auth key: open http://$HOSTNAME.local/ after first boot to log in"
fi
