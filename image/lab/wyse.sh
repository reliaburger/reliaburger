#!/bin/bash
# wyse.sh <n> [fresh]: a "virtual Wyse 3040" on the lab network, standing in
# for the hardware until the last stage (and S3's x86-64 smoke run): x86-64
# q35 under TCG with -cpu Westmere (SSE4.2 and AES-NI but no AVX, like the
# Wyse's Atom x5-Z8350), 2 GiB, a 7.25 GiB disk (the eMMC's ~7.3 GiB, as
# image/tests/disk.sh has it) and Ubuntu's OVMF
# (fetch-ovmf.sh). Real firmware PXE, ProxyDHCP, our iPXE, the installer,
# then a reboot; the disk comes first in the boot order, so the same QEMU
# process then boots the installed system. MAC 52:54:00:00:02:0<n>, from the
# router's pool (no reservation). Runs in the background.
set -euo pipefail; . "$(dirname "$0")/lab.env"; cd "$WORK"
n=${1:?usage: wyse.sh <n> [fresh]}; hex=$(printf %02x "$n")
[ -f OVMF_CODE_4M.fd ] || { echo "no OVMF yet; run fetch-ovmf.sh first" >&2; exit 2; }
if [ "${2:-}" = fresh ]; then rm -f "wyse$n.qcow2" "wyse$n-vars.fd"; fi
[ -f "wyse$n.qcow2" ] || qemu-img create -q -f qcow2 "wyse$n.qcow2" 7424M
[ -f "wyse$n-vars.fd" ] || { cp OVMF_VARS_4M.fd "wyse$n-vars.fd"; chmod u+w "wyse$n-vars.fd"; }
# A fresh hub slot per QEMU start (see rbnode.sh).
slot=$(cat slot.next 2>/dev/null || echo 1); echo $((slot + 1)) > slot.next
[ "$slot" -le "$SLOTS" ] || { echo "out of hub slots ($SLOTS); restart the server" >&2; exit 2; }
creds=(-smbios "type=11,value=io.systemd.credential:ssh.authorized_keys.root=$(cat l2lab_key.pub)")
seed="$WORK/cluster/seeds/wyse-$(printf %02d "$n").tgz"
[ -f "$seed" ] && creds+=(-smbios "type=11,value=io.systemd.credential.binary:reliaburger.seed=$(base64 < "$seed" | tr -d '\n')")
: > "wyse$n.serial.log"
# STICK=<image> plugs in a seed stick (make-seed-stick.sh) as USB storage.
# NOSSH=1: no SSH credential, so only a seed's key can grant SSH.
[ -n "${NOSSH:-}" ] && creds=("${creds[@]:2}")
# NETFIRST=1: network boot before the disk, as an operator might leave it.
[ -n "${NETFIRST:-}" ] && { DISKIDX=2; NETIDX=1; }
[ -n "${STICK:-}" ] && creds+=(-device qemu-xhci -device usb-storage,drive=stick,removable=on -drive "if=none,id=stick,format=raw,file=$STICK")
qemu-system-x86_64 -name "wyse$n" -machine q35 -accel tcg,thread=multi -cpu Westmere -smp 4 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="wyse$n-vars.fd" \
    -drive if=none,id=d0,format=qcow2,file="wyse$n.qcow2" -device virtio-blk-pci,drive=d0,bootindex=${DISKIDX:-1} \
    -device virtio-net-pci,netdev=n0,mac=52:54:00:00:02:$hex,bootindex=${NETIDX:-2} \
    -netdev "stream,id=n0,server=off,addr.type=unix,addr.path=sock/l2-$slot.sock,reconnect-ms=1000" \
    -device virtio-rng-pci "${creds[@]}" \
    -display none -monitor none -serial file:"wyse$n.serial.log" -pidfile "wyse$n.pid" </dev/null >>qemu-stderr.log 2>&1 &
sleep 1; echo "wyse $n running on hub slot $slot, pid $(cat "wyse$n.pid"), log work/wyse$n.serial.log"
