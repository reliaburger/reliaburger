#!/bin/bash
# relish netboot installs a machine (x86-64, KVM): run by
# .github/workflows/appliance.yml on lab builds, with the commit's relish.
#
#   relish-netboot-install.sh <artefact dir> <version> <relish> [dhcp-proxy]
#
# The LAN is a bridge. A network namespace on it plays the home router: its
# dnsmasq hands out addresses and knows nothing about booting. The runner
# runs `relish netboot` on the bridge as a ProxyDHCP, TFTP and HTTP server,
# exactly as an operator's laptop would.
#
# With `dhcp-proxy`, it's the isolated lab's split instead (image/lab/mac/):
# there's no router namespace, the lab Mac's own dnsmasq.conf runs on the
# runner beside relish (with CI's bridge and subnet), owns UDP 67 and sends
# option 60 PXEClient, and `relish netboot --mode-dhcp-proxy` answers on UDP
# 4011 only. The test then also wants the firmware's and iPXE's requests
# answered on 4011. A VM whose disk already holds a GPT
# and an ext4 filesystem, like a used Wyse with ThinOS on it, boots from the
# network (disk first, so the second boot starts the installed system). The
# installer reports the used disk, and relish wipes it because --wipe lists
# the VM's MAC: there's no terminal here to answer the question. The test
# passes once bun is healthy on it, relish has remembered the machine as
# installed, relish's log shows it decided to wipe, and relish's start-up
# probe saw the router's DHCP.
# Needs root (the bridge, port 67, a loop device), qemu-system-x86, ovmf,
# dnsmasq, sfdisk, e2fsprogs and /dev/kvm.
set -euo pipefail
# shellcheck source=image/tests/disk.sh
. "$(dirname "$0")/disk.sh"
out=$(cd "${1:?usage: relish-netboot-install.sh <artefact dir> <version> <relish>}" && pwd)
version=${2:?usage}
relish=$(readlink -f "${3:?usage}")
mode=${4:-router}
case "$mode" in router|dhcp-proxy) ;; *) echo "unknown mode $mode"; exit 1 ;; esac
work=$(mktemp -d)
mac=52:54:00:42:00:21
[ -f "$out/reliaburger-os_$version.raw.zst" ] || { echo "no $version image in $out"; exit 1; }

sudo ip link add rbbr0 type bridge
sudo ip addr add 10.42.0.1/24 dev rbbr0
sudo ip link set rbbr0 up
cleanup() {
    [ -n "${qemu:-}" ] && kill "$qemu" 2>/dev/null || true
    [ -n "${server:-}" ] && sudo kill "$server" 2>/dev/null || true
    [ -f "$work/dnsmasq.pid" ] && sudo kill "$(cat "$work/dnsmasq.pid")" 2>/dev/null || true
    sudo ip netns pids rbrouter 2>/dev/null | xargs -r sudo kill 2>/dev/null || true
    sudo ip netns del rbrouter 2>/dev/null || true
    sudo ip link del rbtap0 2>/dev/null || true
    sudo ip link del rbveth0 2>/dev/null || true
    sudo ip link del rbbr0 2>/dev/null || true
}
trap cleanup EXIT
if [ "$mode" = router ]; then
    sudo ip netns add rbrouter
    sudo ip link add rbveth0 type veth peer name rbveth1
    sudo ip link set rbveth1 netns rbrouter
    sudo ip link set rbveth0 master rbbr0 up
    sudo ip -n rbrouter addr add 10.42.0.2/24 dev rbveth1
    sudo ip -n rbrouter link set rbveth1 up
    sudo ip netns exec rbrouter dnsmasq --interface=rbveth1 --bind-interfaces --port=0 \
        --dhcp-range=10.42.0.100,10.42.0.200,12h --dhcp-option=3,10.42.0.1 \
        --pid-file="$work/dnsmasq.pid" --log-dhcp --log-facility="$work/dnsmasq.log"
    netboot_mode=()
else
    # The lab Mac's file, moved onto CI's bridge and subnet: the runner is
    # the Mac, at the subnet's .1.
    sed -e 's/^interface=en7$/interface=rbbr0/' -e 's/10\.77\.0\./10.42.0./g' \
        "$(dirname "$0")/../lab/mac/dnsmasq.conf" > "$work/lab-dnsmasq.conf"
    grep -q '^interface=rbbr0$' "$work/lab-dnsmasq.conf"
    sudo dnsmasq --conf-file="$work/lab-dnsmasq.conf" \
        --pid-file="$work/dnsmasq.pid" --log-facility="$work/dnsmasq.log"
    netboot_mode=(--mode-dhcp-proxy)
fi

# The layout relish netboot serves: <dir>/<arch>/, iPXE in netboot/.
mkdir -p "$work/serve"
ln -s "$out" "$work/serve/x86_64"
# The log is ours to read, so the redirect stays outside sudo.
# shellcheck disable=SC2024
sudo "$relish" netboot "$work/serve" --key "$out/spike-signing-key.pub.pem" \
    --interface rbbr0 --mac "$mac" --wipe "$mac" --for 30m "${netboot_mode[@]}" \
    < /dev/null > "$work/netboot.log" 2>&1 &
server=$!

sudo ip tuntap add rbtap0 mode tap user "$(id -un)"
sudo ip link set rbtap0 master rbbr0 up
# A used disk: the Wyse-sized test disk (disk.sh), then a GPT with one
# partition holding an ext4 filesystem.
blank_disk "$work/used.raw" "$out/reliaburger-os_$version.raw.zst"
printf 'label: gpt\nsize=1G, type=L, name=ThinOS\n' | sfdisk -q "$work/used.raw"
loop=$(sudo losetup -fP --show "$work/used.raw")
sudo mkfs.ext4 -q -L ThinOS "${loop}p1"
sudo losetup -d "$loop"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
log="$work/serial.log"
qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$work/vars.fd" \
    -drive if=none,id=d0,format=raw,file="$work/used.raw" \
    -device virtio-blk-pci,drive=d0,bootindex=1 \
    -netdev tap,id=n0,ifname=rbtap0,script=no,downscript=no \
    -device "virtio-net-pci,netdev=n0,mac=$mac,bootindex=2" \
    -display none -serial "file:$log" &
qemu=$!
start=$(date +%s)
result=timeout
while [ $(( $(date +%s) - start )) -lt 900 ]; do
    if grep -q -e "reliaburger: bun healthy" -e "reliaburger: unclaimed" "$log" 2>/dev/null; then result=pass; break; fi
    if grep -q -e "reliaburger-install: FAILED" -e "reliaburger: bun not healthy" "$log" 2>/dev/null; then result=fail; break; fi
    if ! kill -0 "$qemu" 2>/dev/null; then result=exited; break; fi
    if ! sudo kill -0 "$server" 2>/dev/null; then result="relish netboot stopped"; break; fi
    sleep 5
done
elapsed=$(( $(date +%s) - start ))
if [ "$result" = pass ] && ! sudo grep -qi "$mac" "$work/serve/netboot-installed.json" 2>/dev/null; then
    result="installed, but relish netboot didn't remember the machine"
fi
if [ "$result" = pass ] && ! grep -q "$mac.*: disk .*: wiping it and installing (--wipe lists it)" "$work/netboot.log"; then
    result="installed, but relish netboot's log doesn't show the used disk being wiped"
fi
# Its start-up probe should have seen the router's dnsmasq hand out addresses.
if [ "$mode" = router ] && [ "$result" = pass ] && ! grep -q "hands out addresses on rbbr0" "$work/netboot.log"; then
    result="installed, but relish netboot's probe didn't see the router's DHCP"
fi
# Beside its own DHCP server, relish should have known it was there, and
# answered the firmware and iPXE on UDP 4011, never on 67.
if [ "$mode" = dhcp-proxy ] && [ "$result" = pass ]; then
    if ! grep -q -e "this machine's DHCP server (10.42.0.1) hands out addresses on rbbr0" \
            -e "a DHCP server on this machine holds UDP 67" "$work/netboot.log"; then
        result="installed, but relish netboot didn't see the DHCP server on its own machine"
    elif ! grep "$mac (" "$work/netboot.log" | grep -v "(iPXE)" | grep -q ": ack on 4011,"; then
        result="installed, but the firmware wasn't answered on UDP 4011"
    elif ! grep -q "$mac (iPXE): ack on 4011," "$work/netboot.log"; then
        result="installed, but iPXE wasn't answered on UDP 4011"
    elif grep -q ": offer, boot" "$work/netboot.log"; then
        result="installed, but relish answered on UDP 67 beside the local DHCP server"
    fi
fi
echo "--- serial console ---"
cat "$log" || true
echo "--- relish netboot ---"
cat "$work/netboot.log" || true
echo "--- dnsmasq ($mode) ---"
sudo cat "$work/dnsmasq.log" || true
{
    echo "### relish netboot install ($mode): $result after ${elapsed} s"
    echo '```'
    cat "$work/netboot.log" || true
    grep -a -e "reliaburger" "$log" | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
[ "$result" = pass ]
