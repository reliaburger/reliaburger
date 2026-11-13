#!/bin/bash
# Two appliances form a cluster from their seeds alone (x86-64, KVM): run by
# .github/workflows/appliance.yml on lab builds, whose bun is the commit's.
#
#   seeded-pair.sh <artefact dir> <version> <relish>
#
# A bridge on the runner is the LAN: dnsmasq hands each VM its reserved
# address, and the runner itself (10.42.0.1) is the operator. `relish cluster
# create --bare-metal` makes the cluster and both seeds. Node 1 reads its
# create seed from an RBSEED stick (a FAT image on a USB drive, made by
# image/lab/make-seed-stick.sh), as a real machine would; node 2 gets its
# join seed as the reliaburger.seed credential. Node 2 enrols with its token,
# fetches the master key with its new certificate, and the test passes when
# relish sees both nodes alive, node 1 says its seed came from the stick and
# was wiped from it, and no byte run of that seed is left on the stick
# (seed_wiped.py), since it carried the master key.
# Needs root (the bridge), qemu-system-x86, qemu-utils, mtools, ovmf, dnsmasq
# and /dev/kvm.
set -euo pipefail
# shellcheck source=image/tests/disk.sh
. "$(dirname "$0")/disk.sh"
out=$(cd "${1:?usage: seeded-pair.sh <artefact dir> <version> <relish>}" && pwd)
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
# A throwaway key, so a failure can read each node's journal over SSH.
ssh-keygen -q -t ed25519 -N "" -f "$work/ssh-key"
"$relish" cluster create --bare-metal "$work/cluster" --name pair --operator 10.42.0.1 --yes \
    --network 10.42.0.0/24 --ssh-key "$work/ssh-key.pub" \
    "${macs[0]}@${ips[0]}" "${macs[1]}@${ips[1]}"

stick_name="seeds/$(echo "${macs[0]}" | tr : -).seed"
mkdir -p "$work/stick/seeds"
cp "$work/cluster/stick/$stick_name" "$work/stick/seeds/"
"$(dirname "$0")/../lab/make-seed-stick.sh" "$work/stick.img" "$work/stick"

pids=()
for i in 0 1; do
    sudo ip tuntap add "rbtap$i" mode tap user "$(id -un)"
    sudo ip link set "rbtap$i" master rbbr0 up
    zstd -q -d "$out/reliaburger-os_$version.raw.zst" -o "$work/disk$i.raw"
    disk_from_image "$work/disk$i.raw"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars$i.fd"
    if [ "$i" = 0 ]; then
        seed_from=(-device qemu-xhci
            -drive "if=none,id=stick,format=raw,file=$work/stick.img"
            -device usb-storage,drive=stick,removable=on)
    else
        seed="$work/cluster/stick/seeds/$(echo "${macs[$i]}" | tr : -).seed"
        seed_from=(-smbios "type=11,value=io.systemd.credential.binary:reliaburger.seed=$(base64 -w0 < "$seed")")
    fi
    qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
        -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
        -drive if=pflash,format=raw,file="$work/vars$i.fd" \
        -drive if=virtio,format=raw,file="$work/disk$i.raw" \
        -netdev "tap,id=n0,ifname=rbtap$i,script=no,downscript=no" \
        -device "virtio-net-pci,netdev=n0,mac=${macs[$i]}" \
        "${seed_from[@]}" \
        -display none -serial "file:$work/node$i.log" &
    pids+=($!)
done

start=$(date +%s)
result=timeout
while [ $(( $(date +%s) - start )) -lt 900 ]; do
    if grep -q "reliaburger: bun healthy" "$work/node1.log" 2>/dev/null \
        && nodes=$("$relish" nodes 2>/dev/null) \
        && [ "$(echo "$nodes" | grep -c alive)" = 2 ]; then
        result=pass
        break
    fi
    if grep -q -e "enrolling as .* failed for good" -e "reliaburger: bun not healthy" "$work"/node*.log 2>/dev/null; then
        result=fail
        break
    fi
    sleep 5
done
elapsed=$(( $(date +%s) - start ))
if [ "$result" = pass ]; then
    for line in "reliaburger: seed from the RBSEED stick ($stick_name)" \
        "reliaburger: wiped $stick_name from the RBSEED stick: it held the master key"; do
        if ! grep -qF "$line" "$work/node0.log"; then
            echo "node 1 never said: $line"
            result=fail
        fi
    done
    if [ "$result" = pass ] \
        && ! python3 "$(dirname "$0")/seed_wiped.py" "$work/stick.img" "$work/cluster/stick/$stick_name"; then
        result=fail
    fi
fi
for i in 0 1; do
    echo "--- node $((i + 1)) console ---"
    cat "$work/node$i.log" || true
done
if [ "$result" != pass ]; then
    # The cluster's own view, from node 1, which answers when node 2 won't.
    for command in nodes council events; do
        echo "--- relish $command ---"
        "$relish" "$command" 2>&1 || true
    done
    for i in 0 1; do
        echo "--- node $((i + 1)) bun journal ---"
        timeout 30 ssh -i "$work/ssh-key" -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 "root@${ips[$i]}" \
            journalctl -b -o short-monotonic --no-pager -u reliaburger.service 2>&1 \
            | grep -v "snapshot collection failed" | tail -n 150 || true
    done
fi
{
    echo "### Seeded pair: $result after ${elapsed} s"
    echo '```'
    "$relish" nodes 2>&1 || true
    grep -a -h "reliaburger:" "$work"/node*.log | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "$summary"
[ "$result" = pass ]
