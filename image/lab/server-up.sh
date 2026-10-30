#!/bin/bash
# server-up.sh: start the router and netboot server VM (Ubuntu 26.04 arm64,
# 2 GiB, HVF) in the background. NIC 1 is slirp "wan" (internet, and SSH on
# 127.0.0.1:$SSH_PORT); NIC 2 is "lan" on QEMU hub 0, which is also wired to
# $SLOTS unix-socket stream servers (work/sock/l2-<n>.sock) that the nodes
# connect to. It resets the slot counter (see rbnode.sh).
set -euo pipefail; . "$(dirname "$0")/lab.env"; cd "$WORK"
[ -f server.qcow2 ] || qemu-img create -q -f qcow2 -F qcow2 -b ubuntu-26.04-server-cloudimg-arm64.img server.qcow2 20G
[ -f seed.img ] || "$LAB/mkseed.sh" >/dev/null
mkdir -p sock; rm -f sock/l2-*.sock; echo 1 > slot.next
hub=(); for i in $(seq 1 "$SLOTS"); do
    hub+=(-netdev "stream,id=s$i,server=on,addr.type=unix,addr.path=sock/l2-$i.sock"
          -netdev "hubport,id=hp$i,hubid=0,netdev=s$i")
done
[ -f server-vars.fd ] || { cp "$Q/edk2-arm-vars.fd" server-vars.fd; chmod u+w server-vars.fd; }
# Socket paths stay relative: macOS caps unix socket paths at 104 bytes.
qemu-system-aarch64 -name l2lab-server -machine virt -accel hvf -cpu host -smp 2 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd" \
    -drive if=pflash,format=raw,file=server-vars.fd \
    -drive if=virtio,format=qcow2,file=server.qcow2 \
    -drive if=virtio,format=raw,file=seed.img,readonly=on \
    -device virtio-net-pci,netdev=wan,mac=52:54:00:00:00:10 \
    -netdev user,id=wan,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22 \
    -device virtio-net-pci,netdev=lan,mac=52:54:00:00:00:01 \
    -netdev hubport,id=lan,hubid=0 "${hub[@]}" \
    -display none -monitor none -serial file:server.serial.log \
    -pidfile server.pid </dev/null >>qemu-stderr.log 2>&1 &
sleep 1; echo "server pid $(cat server.pid); ssh: $LAB/ssh.sh"
