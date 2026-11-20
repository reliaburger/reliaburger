#!/bin/bash
# build-server.sh: build the lab server VM from scratch and leave it running.
# It fetches the Ubuntu 26.04 arm64 cloud image (checked against Canonical's
# SHA256SUMS), makes a fresh overlay and seed, and waits for cloud-init to
# install dnsmasq, nftables and OVMF, reboot, and start the lab's services.
set -euo pipefail; . "$(dirname "$0")/lab.env"; cd "$WORK"
base=https://cloud-images.ubuntu.com/releases/resolute/release
img=ubuntu-26.04-server-cloudimg-arm64.img
if [ ! -f "$img" ]; then
    curl -fL -o "$img.part" "$base/$img"
    line=$(curl -fsS "$base/SHA256SUMS" \
        | awk -v f="$img" '{ n = $2; sub(/^\*/, "", n); if (n == f) print $1 "  " f ".part" }')
    [ -n "$line" ] || { echo "$img isn't in $base/SHA256SUMS" >&2; exit 1; }
    echo "$line" | shasum -a 256 -c -
    mv "$img.part" "$img"
fi
rm -f server.qcow2 server-vars.fd server.serial.log
"$LAB/mkseed.sh" >/dev/null
start=$(date +%s)
"$LAB/server-up.sh"
until grep -aq "Cloud-init.*finished" server.serial.log 2>/dev/null; do sleep 2; done
echo "cloud-init done after $(( $(date +%s) - start )) s; waiting for the reboot"
sleep 5
until "$LAB/ssh.sh" -o ConnectTimeout=2 'ip link show wan >/dev/null && systemctl is-active -q l2lab-proxy l2lab-router l2lab-http' 2>/dev/null; do sleep 2; done
echo "server ready after $(( $(date +%s) - start )) s"
