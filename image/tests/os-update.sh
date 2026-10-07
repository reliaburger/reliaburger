#!/bin/bash
# bun rolls an OS update (x86-64, KVM): run by .github/workflows/appliance.yml
# on lab builds, which build a second, newer version for it, and a third,
# broken one for the fallback.
#
#   os-update.sh <artefact dir> <version> <release tree> <target> <relish> [--fallback]
#
# One appliance on the bridge LAN forms a cluster from its seed, on
# <version>, on a 7.25 GiB disk (disk.sh). The runner serves <release tree>,
# which lab-release.sh laid out like GitHub releases: the next version in
# …/releases/download/os-<next>-x86_64/ beside the lab channel in
# …/os-channel/ (and the broken version beside it), all signed with the
# throwaway key the images carry. It's what the run uploads as
# appliance-x86_64-next, so the test serves exactly what the lab gets.
# `relish os upgrade <target>` starts the rollout; the node drains, stages
# the release, lets systemd-sysupdate write the spare slot and reboots into
# it.
#
# Without --fallback, <target> is the next version, which the channel names:
# the test passes when the rollout completes and the node is healthy on it.
#
# With --fallback, <target> is the broken version, whose bun never starts
# (image/tests/broken-bun). Each of its three counted boots fails the boot
# check and reboots; then systemd-boot falls back to <version> by itself.
# The test passes when the node is healthy on <version> again, never was on
# <target>, and the rollout has paused on the fallback bun reported. It
# records how long the fallback took, from the reboot into <target> to bun
# healthy on <version>: the Wyse exit test needs 10 minutes or less (S5).
# Needs root (the bridge), qemu-system-x86, ovmf, dnsmasq and /dev/kvm.
set -euo pipefail
# shellcheck source=image/tests/disk.sh
. "$(dirname "$0")/disk.sh"
usage="usage: os-update.sh <artefact dir> <version> <release tree> <target> <relish> [--fallback]"
out=$(cd "${1:?$usage}" && pwd)
version=${2:?$usage}
tree=$(cd "${3:?$usage}" && pwd)
target=${4:?$usage}
relish=$(readlink -f "${5:?$usage}")
mode=update
case "${6:-}" in
    "") ;;
    --fallback) mode=fallback ;;
    *) echo "$usage" >&2; exit 2 ;;
esac
if [ "$mode" = update ]; then
    # The lab channel names the version the node updates to. relish checks a
    # channel against the release keys only, so `relish os list` warns about
    # this one, and the upgrade names its version.
    grep -q "\"version\":\"$target\"" "$tree/releases/download/os-channel/os-channel.json" \
        || { echo "os-update.sh: the lab channel doesn't name $target" >&2; exit 1; }
else
    [ -f "$tree/releases/download/os-$target-x86_64/reliaburger-os_$target.SHA256SUMS.sig" ] \
        || { echo "os-update.sh: no signed release for $target in $tree" >&2; exit 1; }
fi
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
"$relish" cluster create --bare-metal "$work/cluster" --name solo --operator 10.42.0.1 "$mac@$ip"

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
        sleep 2
    done
}
# How many times the serial console shows a line matching $1.
seen() { grep -a -c -e "$1" "$log" 2>/dev/null || true; }
at_least() { [ "$(seen "$1")" -ge "$2" ]; }
healthy="reliaburger: bun healthy .* on OS"
healthy_on() { at_least "$healthy $1\$" "${2:-1}"; }
on_next() { "$relish" os list --channel "$channel" 2>/dev/null | grep -q "solo-1 *$target"; }
finished() { "$relish" os status 2>/dev/null | tee "$work/status.txt" | grep -q "to $target: complete"; }
# bun on <version> reads os-update.json, finds it isn't on <target> and says
# so; the leader (the same node) pauses the rollout on it.
paused() {
    "$relish" os status 2>/dev/null | tee "$work/status.txt" \
        | grep -q "paused: solo-1: booted $version instead"
}

if [ "$mode" = update ]; then
    if wait_for healthy_on "$version" \
        && "$relish" os list --channel "$channel" \
        && "$relish" os upgrade "$target" --channel "$channel" \
        && wait_for healthy_on "$target" \
        && wait_for on_next \
        && wait_for finished; then
        result=pass
    fi
    outcome="$result"
    title="OS update $version → $target"
else
    # Each boot starts the kernel afresh, so its banner counts them: one
    # before the update, three tries of <target>, one back on <version>.
    t_upgrade='' t_reboot='' t_back=''
    if wait_for healthy_on "$version" \
        && "$relish" os list --channel "$channel" \
        && "$relish" os upgrade "$target" --channel "$channel" \
        && t_upgrade=$(date +%s) \
        && wait_for at_least "Linux version" 2 \
        && t_reboot=$(date +%s) \
        && wait_for healthy_on "$version" 2 \
        && t_back=$(date +%s) \
        && wait_for paused; then
        result=pass
    fi
    tries=$(seen "reliaburger: bun not healthy after [0-9]* s on OS $target")
    on_broken=$(seen "$healthy $target\$")
    boots=$(seen "Linux version")
    # Three failed tries, never healthy on the broken version: anything
    # else isn't the fallback this test is for.
    if [ "$result" = pass ] && { [ "$tries" -ne 3 ] || [ "$on_broken" -ne 0 ]; }; then
        result=fail
    fi
    outcome="$result: $tries failed tries, $boots boots"
    if [ -n "$t_back" ]; then
        fallback=$((t_back - t_reboot))
        outcome="$outcome, fell back in ${fallback} s ($((fallback / 60)) min $((fallback % 60)) s) from the reboot into $target, $((t_back - t_upgrade)) s from \`relish os upgrade\`"
        if [ "$fallback" -gt 600 ]; then
            echo "::warning::the fallback took ${fallback} s, more than the 10 minutes the Wyse exit test allows"
        fi
    fi
    title="OS fallback $version → $target → $version"
fi
elapsed=$(( $(date +%s) - start ))
echo "--- serial console ---"
cat "$log" || true
echo "--- release server ---"
cat "$work/http.log" || true
{
    echo "### $title: $outcome; ${elapsed} s in all"
    echo '```'
    cat "$work/status.txt" 2>/dev/null || true
    grep -a -h -e "reliaburger: bun" -e "reliaburger: counted boot" -e "os update" "$log" || true
    echo '```'
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
[ "$result" = pass ]
