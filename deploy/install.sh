#!/bin/sh
# Copies the cross-compiled binary and the systemd unit to a Raspberry Pi and
# starts the service. Usage: deploy/install.sh pi@zero1
set -eu

host="${1:?usage: $0 user@host}"
dir="$(cd "$(dirname "$0")/.." && pwd)"
bin="$dir/target/arm-unknown-linux-musleabihf/release/tailvision"

if [ ! -x "$bin" ]; then
    echo "binary not found; run: cargo build --release --target arm-unknown-linux-musleabihf" >&2
    exit 1
fi

scp "$bin" "$dir/deploy/tailvision.service" "$dir/deploy/dnsmasq-hotspot.conf" "$dir/deploy/journald-persistent.conf" "$dir/deploy/modules-gadget.conf" "$host:/tmp/"
ssh "$host" 'set -e
    sudo install -m 755 /tmp/tailvision /usr/local/bin/tailvision
    sudo install -m 644 /tmp/tailvision.service /etc/systemd/system/tailvision.service
    sudo install -d /etc/NetworkManager/dnsmasq-shared.d
    sudo install -m 644 /tmp/dnsmasq-hotspot.conf /etc/NetworkManager/dnsmasq-shared.d/tailvision.conf
    sudo install -d /etc/systemd/journald.conf.d
    sudo install -m 644 /tmp/journald-persistent.conf /etc/systemd/journald.conf.d/tailvision.conf
    sudo install -m 644 /tmp/modules-gadget.conf /etc/modules-load.d/tailvision-gadget.conf
    grep -q "dr_mode=peripheral" /boot/firmware/config.txt || printf "\n[all]\ndtoverlay=dwc2,dr_mode=peripheral\n" | sudo tee -a /boot/firmware/config.txt >/dev/null
    rm -f /tmp/tailvision /tmp/tailvision.service /tmp/dnsmasq-hotspot.conf /tmp/journald-persistent.conf /tmp/modules-gadget.conf
    sudo systemctl daemon-reload
    sudo systemctl enable --now tailvision
    sleep 1
    systemctl --no-pager --lines=5 status tailvision'
