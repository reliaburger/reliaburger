#!/bin/bash
# relish netboot installs a machine (x86-64, KVM): run by
# .github/workflows/appliance.yml on lab builds, with the commit's relish.
#
#   relish-netboot-install.sh <artefact dir> <version> <relish>
#
# The LAN is a bridge. A network namespace on it plays the home router: its
# dnsmasq hands out addresses and knows nothing about booting. The runner
# runs `relish netboot` on the bridge as a ProxyDHCP, TFTP and HTTP server,
# exactly as an operator's laptop would. A VM with a blank disk boots from
# the network (disk first, so the second boot starts the installed system),
# and the test passes once bun is healthy on it and relish has remembered
# the machine as installed.
# Needs root (the bridge, port 67), qemu-system-x86, ovmf, dnsmasq and /dev/kvm.
set -euo pipefail
out=$(cd "${1:?usage: relish-netboot-install.sh <artefact dir> <version> <relish>}" && pwd)
version=${2:?usage}
relish=$(readlink -f "${3:?usage}")
work=$(mktemp -d)
mac=52:54:00:42:00:21
[ -f "$out/reliaburger-os_$version.raw.zst" ] || { echo "no $version image in $out"; exit 1; }

sudo ip link add rbbr0 type bridge
sudo ip addr add 10.42.0.1/24 dev rbbr0
sudo ip link set rbbr0 up
sudo ip netns add rbrouter
sudo ip link add rbveth0 type veth peer name rbveth1
sudo ip link set rbveth1 netns rbrouter
sudo ip link set rbveth0 master rbbr0 up
sudo ip -n rbrouter addr add 10.42.0.2/24 dev rbveth1
sudo ip -n rbrouter link set rbveth1 up
cleanup() {
    [ -n "${qemu:-}" ] && kill "$qemu" 2>/dev/null || true
    [ -n "${server:-}" ] && sudo kill "$server" 2>/dev/null || true
    sudo ip netns pids rbrouter 2>/dev/null | xargs -r sudo kill 2>/dev/null || true
    sudo ip netns del rbrouter 2>/dev/null || true
    sudo ip link del rbtap0 2>/dev/null || true
    sudo ip link del rbveth0 2>/dev/null || true
    sudo ip link del rbbr0 2>/dev/null || true
}
trap cleanup EXIT
sudo ip netns exec rbrouter dnsmasq --interface=rbveth1 --bind-interfaces --port=0 \
    --dhcp-range=10.42.0.100,10.42.0.200,12h --dhcp-option=3,10.42.0.1 \
    --pid-file="$work/dnsmasq.pid" --log-dhcp --log-facility="$work/dnsmasq.log"

# The layout relish netboot serves: <dir>/<arch>/, iPXE in netboot/.
mkdir -p "$work/serve"
ln -s "$out" "$work/serve/x86_64"
# The log is ours to read, so the redirect stays outside sudo.
# shellcheck disable=SC2024
sudo "$relish" netboot "$work/serve" --key "$out/spike-signing-key.pub.pem" \
    --interface rbbr0 --mac "$mac" --for 30m > "$work/netboot.log" 2>&1 &
server=$!

sudo ip tuntap add rbtap0 mode tap user "$(id -un)"
sudo ip link set rbtap0 master rbbr0 up
truncate -s 8G "$work/blank.raw"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
log="$work/serial.log"
qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$work/vars.fd" \
    -drive if=none,id=d0,format=raw,file="$work/blank.raw" \
    -device virtio-blk-pci,drive=d0,bootindex=1 \
    -netdev tap,id=n0,ifname=rbtap0,script=no,downscript=no \
    -device "virtio-net-pci,netdev=n0,mac=$mac,bootindex=2" \
    -display none -serial "file:$log" &
qemu=$!
start=$(date +%s)
result=timeout
while [ $(( $(date +%s) - start )) -lt 900 ]; do
    if grep -q "reliaburger: bun healthy" "$log" 2>/dev/null; then result=pass; break; fi
    if grep -q -e "reliaburger-install: FAILED" -e "reliaburger: bun not healthy" "$log" 2>/dev/null; then result=fail; break; fi
    if ! kill -0 "$qemu" 2>/dev/null; then result=exited; break; fi
    if ! sudo kill -0 "$server" 2>/dev/null; then result="relish netboot stopped"; break; fi
    sleep 5
done
elapsed=$(( $(date +%s) - start ))
if [ "$result" = pass ] && ! sudo grep -qi "$mac" "$work/serve/netboot-installed.json" 2>/dev/null; then
    result="installed, but relish netboot didn't remember the machine"
fi
echo "--- serial console ---"
cat "$log" || true
echo "--- relish netboot ---"
cat "$work/netboot.log" || true
echo "--- router dnsmasq ---"
sudo cat "$work/dnsmasq.log" || true
{
    echo "### relish netboot install: $result after ${elapsed} s"
    echo '```'
    cat "$work/netboot.log" || true
    grep -a -e "reliaburger" "$log" | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
[ "$result" = pass ]
