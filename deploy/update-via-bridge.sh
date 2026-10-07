#!/bin/bash
# Fast update of a tailvision card that sits in a microsd-ota bridge: instead
# of re-flashing the whole image (every image rebuild shuffles the ext4
# layout, so a delta flash still moves hundreds of MB over the 3 Mbaud
# link), expose the card over NBD and copy only the binary and unit files.
#
#   deploy/update-via-bridge.sh [-p /dev/ttyUSB1]
#
# The Pi must be powered OFF (the bridge cannot tell, so this takes the PC
# lock by force). Needs sudo for the loop/NBD mount.
set -euo pipefail

port=/dev/ttyUSB1
while [ $# -gt 0 ]; do
    case "$1" in
        -p) port=$2; shift 2 ;;
        *) echo "usage: $0 [-p PORT]" >&2; exit 1 ;;
    esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/arm-unknown-linux-musleabihf/release/tailvision"
sdusb="$(command -v sdusb || ls "$root"/../microsd-ota/gateware/tools/sdusb/target/release/sdusb 2>/dev/null || true)"
[ -x "$bin" ] || { echo "build first: cargo build --release --target arm-unknown-linux-musleabihf" >&2; exit 1; }
[ -x "$sdusb" ] || { echo "sdusb not found (build microsd-ota/gateware/tools/sdusb)" >&2; exit 1; }

mnt="$(mktemp -d)"
cleanup() {
    set +e
    sudo umount "$mnt" 2>/dev/null
    sudo pkill -f "sdusb -p $port nbd" 2>/dev/null
    sleep 1
    "$sdusb" -p "$port" unlock >/dev/null 2>&1
    rmdir "$mnt" 2>/dev/null
}
trap cleanup EXIT

sudo modprobe nbd max_part=8
sudo "$sdusb" -p "$port" --force nbd /dev/nbd0 &
sleep 4
sudo partprobe /dev/nbd0
sudo mount /dev/nbd0p2 "$mnt"
echo ">> installing $(basename "$bin") and units"
sudo install -m 755 "$bin" "$mnt/usr/local/bin/tailvision"
sudo install -m 644 "$root/deploy/tailvision.service" "$mnt/etc/systemd/system/tailvision.service"
sudo install -m 644 "$root/deploy/tailscale-firstboot.service" "$mnt/etc/systemd/system/tailscale-firstboot.service"
sudo install -m 644 "$root/deploy/dnsmasq-hotspot.conf" "$mnt/etc/NetworkManager/dnsmasq-shared.d/tailvision.conf"
sync
sudo umount "$mnt"
echo "done: power the Pi on"
