#!/bin/bash
# Two unseeded appliances are claimed over the LAN (x86-64, KVM): run by
# .github/workflows/appliance.yml on lab builds, whose bun is the commit's.
#
#   claimed-pair.sh <artefact dir> <version> <relish>
#
# The same bridge LAN as seeded-pair.sh, but the machines boot with no seed
# and wait to be claimed. `relish machines` must find both over mDNS; node 1
# is claimed by MAC with --create, and no --network, so node 2, claimed by
# address afterwards, can only enrol through the join window relish opens on
# node 1's firewall. The test passes when relish sees both nodes alive and
# the council reports the default appliance size, five voters, which
# --create recorded in fleet.json and node 1 committed when it bootstrapped.
# Needs root (the bridge), qemu-system-x86, ovmf, dnsmasq and /dev/kvm.
set -euo pipefail
# shellcheck source=image/tests/disk.sh
. "$(dirname "$0")/disk.sh"
out=$(cd "${1:?usage: claimed-pair.sh <artefact dir> <version> <relish>}" && pwd)
version=${2:?usage}
relish=$(readlink -f "${3:?usage}")
work=$(mktemp -d)
summary=${GITHUB_STEP_SUMMARY:-/dev/stdout}
macs=(52:54:00:42:00:01 52:54:00:42:00:02)
ips=(10.42.0.11 10.42.0.12)

sudo ip link add rbbr0 type bridge
sudo ip addr add 10.42.0.1/24 dev rbbr0
sudo ip link set rbbr0 up
cleanup() {
    for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    sudo pkill -f "dnsmasq.*rbbr0" || true
    for i in 0 1; do sudo ip link del "rbtap$i" 2>/dev/null || true; done
    sudo ip link del rbbr0 2>/dev/null || true
}
trap cleanup EXIT
sudo dnsmasq --interface=rbbr0 --bind-interfaces --port=0 \
    --dhcp-range=10.42.0.100,10.42.0.200,12h --dhcp-option=3,10.42.0.1 \
    --dhcp-host="${macs[0]},${ips[0]}" --dhcp-host="${macs[1]},${ips[1]}" \
    --pid-file="$work/dnsmasq.pid" --log-facility="$work/dnsmasq.log"

export RELIABURGER_HOME="$work/home"
mkdir -p "$RELIABURGER_HOME"

pids=()
for i in 0 1; do
    sudo ip tuntap add "rbtap$i" mode tap user "$(id -un)"
    sudo ip link set "rbtap$i" master rbbr0 up
    zstd -q -d "$out/reliaburger-os_$version.raw.zst" -o "$work/disk$i.raw"
    disk_from_image "$work/disk$i.raw"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars$i.fd"
    qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
        -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
        -drive if=pflash,format=raw,file="$work/vars$i.fd" \
        -drive if=virtio,format=raw,file="$work/disk$i.raw" \
        -netdev "tap,id=n0,ifname=rbtap$i,script=no,downscript=no" \
        -device "virtio-net-pci,netdev=n0,mac=${macs[$i]}" \
        -display none -serial "file:$work/node$i.log" &
    pids+=($!)
done

start=$(date +%s)
deadline=$((start + 900))
result=fail
# Wait until $1 succeeds, or the deadline passes.
wait_for() {
    until "$@" >/dev/null 2>&1; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 5
    done
}
alive() { [ "$("$relish" nodes 2>/dev/null | grep -c alive)" = "$1" ]; }
listed() { "$relish" machines --wait 3 | tee "$work/machines.txt" | grep -q "${macs[0]}" && grep -q "${macs[1]}" "$work/machines.txt"; }
# The council size --create defaults to: in fleet.json, and in council state.
sized() { grep -q '"council_size": 5' "$work/cluster/fleet.json" \
    && "$relish" council --output json 2>/dev/null | grep -q '"council_size": 5'; }
if wait_for listed \
    && "$relish" machines claim "$work/cluster" --create --name pair --operator 10.42.0.1 \
        --trust-lan "${macs[0]}" \
    && wait_for alive 1 \
    && wait_for sized \
    && "$relish" machines claim "$work/cluster" --trust-lan "${ips[1]}" \
    && wait_for alive 2; then
    result=pass
fi
elapsed=$(( $(date +%s) - start ))
for i in 0 1; do
    echo "--- node $((i + 1)) console ---"
    cat "$work/node$i.log" || true
done
{
    echo "### Claimed pair: $result after ${elapsed} s"
    echo '```'
    cat "$work/machines.txt" 2>/dev/null || true
    "$relish" nodes 2>&1 || true
    "$relish" council 2>&1 || true
    grep -a -h "reliaburger:" "$work"/node*.log | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "$summary"
[ "$result" = pass ]
