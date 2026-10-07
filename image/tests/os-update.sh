#!/bin/bash
# bun rolls an OS update (x86-64, KVM): run by .github/workflows/appliance.yml
# on lab builds, which build a second, newer version for it.
#
#   os-update.sh <artefact dir> <version> <release tree> <next version> <relish>
#
# One appliance on the bridge LAN forms a cluster from its seed, on
# <version>, on a 7.25 GiB disk (disk.sh). The runner serves <release tree>,
# which lab-release.sh laid out like GitHub releases: <next version> in
# …/releases/download/os-<next>-x86_64/ beside the lab channel in
# …/os-channel/, all signed with the throwaway key both images carry. It's
# what the run uploads as appliance-x86_64-next, so the test serves exactly
# what the lab gets. `relish os upgrade` starts the rollout; the node
# drains, stages the release, lets systemd-sysupdate write the spare slot,
# reboots into it and passes its boot checks. The test passes when the
# rollout completes and the node is healthy on <next version>.
# Needs root (the bridge), qemu-system-x86, ovmf, dnsmasq and /dev/kvm.
set -euo pipefail
# shellcheck source=image/tests/disk.sh
. "$(dirname "$0")/disk.sh"
usage="usage: os-update.sh <artefact dir> <version> <release tree> <next version> <relish>"
out=$(cd "${1:?$usage}" && pwd)
version=${2:?$usage}
tree=$(cd "${3:?$usage}" && pwd)
next=${4:?$usage}
relish=$(readlink -f "${5:?$usage}")
# The lab channel names the version the node updates to, signed with the
# run's throwaway key (lab-signing-key.pub.pem, beside it). relish reads it
# with --key, so `relish os list` must name $next as the newest, and the
# upgrade takes the newest without naming it, as the Wyse lab does.
grep -q "\"version\":\"$next\"" "$tree/releases/download/os-channel/os-channel.json" \
    || { echo "os-update.sh: the lab channel doesn't name $next" >&2; exit 1; }
key="$tree/lab-signing-key.pub.pem"
work=$(mktemp -d)
mac=52:54:00:42:00:31
ip=10.42.0.31

sudo ip link add rbbr0 type bridge
sudo ip addr add 10.42.0.1/24 dev rbbr0
sudo ip link set rbbr0 up
cleanup() {
    [ -n "${qemu:-}" ] && kill "$qemu" 2>/dev/null || true
    [ -n "${http:-}" ] && kill "$http" 2>/dev/null || true
    sudo pkill -f "dnsmasq.*rbbr0" || true
    sudo ip link del rbtap0 2>/dev/null || true
    sudo ip link del rbbr0 2>/dev/null || true
}
trap cleanup EXIT
sudo dnsmasq --interface=rbbr0 --bind-interfaces --port=0 \
    --dhcp-range=10.42.0.100,10.42.0.200,12h --dhcp-option=3,10.42.0.1 \
    --dhcp-host="$mac,$ip" --pid-file="$work/dnsmasq.pid" --log-facility="$work/dnsmasq.log"

# The release, where the node looks for it beside the channel.
ls -lR "$tree/releases/download"
(cd "$tree" && exec python3 -m http.server 8000 --bind 10.42.0.1 >"$work/http.log" 2>&1) &
http=$!
channel="http://10.42.0.1:8000/releases/download/os-channel/os-channel.json"

export RELIABURGER_HOME="$work/home"
mkdir -p "$RELIABURGER_HOME"
"$relish" cluster create --bare-metal "$work/cluster" --name solo --operator 10.42.0.1 --yes "$mac@$ip"

sudo ip tuntap add rbtap0 mode tap user "$(id -un)"
sudo ip link set rbtap0 master rbbr0 up
zstd -q -d "$out/reliaburger-os_$version.raw.zst" -o "$work/disk.raw"
disk_from_image "$work/disk.raw"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
seed="$work/cluster/stick/seeds/$(echo "$mac" | tr : -).seed"
log="$work/serial.log"
qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$work/vars.fd" \
    -drive if=virtio,format=raw,file="$work/disk.raw" \
    -netdev tap,id=n0,ifname=rbtap0,script=no,downscript=no \
    -device "virtio-net-pci,netdev=n0,mac=$mac" \
    -smbios "type=11,value=io.systemd.credential.binary:reliaburger.seed=$(base64 -w0 < "$seed")" \
    -display none -serial "file:$log" &
qemu=$!

start=$(date +%s)
deadline=$((start + 1500))
result=fail
wait_for() {
    until "$@" >/dev/null 2>&1; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        kill -0 "$qemu" 2>/dev/null || return 1
        sleep 5
    done
}
healthy_on() { grep -a -q "reliaburger: bun healthy .* on OS $1" "$log"; }
on_next() { "$relish" os list --channel "$channel" --key "$key" 2>/dev/null | grep -q "solo-1 *$next"; }
newest_is_next() { "$relish" os list --channel "$channel" --key "$key" | grep -x "Newest OS release: $next" >/dev/null; }
finished() { "$relish" os status 2>/dev/null | tee "$work/status.txt" | grep -q "to $next: complete"; }
if wait_for healthy_on "$version" \
    && newest_is_next \
    && "$relish" os upgrade --channel "$channel" --key "$key" \
    && wait_for healthy_on "$next" \
    && wait_for on_next \
    && wait_for finished; then
    result=pass
fi
elapsed=$(( $(date +%s) - start ))
echo "--- serial console ---"
cat "$log" || true
echo "--- release server ---"
cat "$work/http.log" || true
{
    echo "### OS update $version → $next: $result after ${elapsed} s"
    echo '```'
    cat "$work/status.txt" 2>/dev/null || true
    grep -a -h -e "reliaburger:" -e "os update" "$log" | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
[ "$result" = pass ]
