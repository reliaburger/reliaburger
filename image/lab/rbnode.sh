#!/bin/bash
# rbnode.sh <n> install|run [fresh]: an aarch64 Reliaburger appliance node on
# the lab network (spike S2-S4). MAC 52:54:00:00:01:0<n>, which the router
# reserves as 192.168.105.10<n>; a 10 GB disk (work/rb<n>.qcow2); 2 GiB; HVF.
#   install  starts our pinned iPXE with -kernel, because Homebrew's edk2 has
#            no network boot under HVF on Apple silicon (no RNG, so its
#            network stack doesn't load). iPXE's embedded script takes the
#            ProxyDHCP answer (192.168.105.2), runs boot.ipxe and chains the
#            installer over HTTP; the installer streams the image to disk and
#            reboots, which -no-reboot turns into QEMU exiting.
#   run      boots the installed disk in the background, passing the lab's
#            SSH key and, if work/cluster/seeds/node-0<n>.tgz exists, the
#            node's seed, as SMBIOS type 11 systemd credentials.
set -euo pipefail; . "$(dirname "$0")/lab.env"; cd "$WORK"
n=${1:?usage: rbnode.sh <n> install|run [fresh]}; what=${2:?install or run}; hex=$(printf %02x "$n")
if [ "${3:-}" = fresh ]; then rm -f "rb$n.qcow2" "rb$n-vars.fd"; fi
[ -f "rb$n.qcow2" ] || qemu-img create -q -f qcow2 "rb$n.qcow2" 10G
[ -f "rb$n-vars.fd" ] || { cp "$Q/edk2-arm-vars.fd" "rb$n-vars.fd"; chmod u+w "rb$n-vars.fd"; }
# Every QEMU start takes a fresh hub slot: the hub's stream server doesn't
# deliver frames to a second client on a reused slot. server-up.sh resets
# the counter.
slot=$(cat slot.next 2>/dev/null || echo 1); echo $((slot + 1)) > slot.next
[ "$slot" -le "$SLOTS" ] || { echo "out of hub slots ($SLOTS); restart the server" >&2; exit 2; }
common=(-machine virt -accel hvf -cpu host -smp 2 -m 2048
    -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd"
    -drive if=pflash,format=raw,file="rb$n-vars.fd"
    -device virtio-net-pci,netdev=n0,mac=52:54:00:00:01:$hex,romfile=
    -netdev "stream,id=n0,server=off,addr.type=unix,addr.path=sock/l2-$slot.sock,reconnect-ms=1000"
    -drive if=none,id=d0,format=qcow2,file="rb$n.qcow2" -device virtio-blk-pci,drive=d0,bootindex=1
    -display none -monitor none)
case $what in
install)
    [ -f ipxe-kernel-arm64.efi ] || { echo "no iPXE yet; run stage-artefacts.sh first" >&2; exit 2; }
    : > "rb$n.install.log"
    start=$(date +%s)
    qemu-system-aarch64 -name "rb$n-install" "${common[@]}" -kernel "$WORK/ipxe-kernel-arm64.efi" \
        -serial file:"rb$n.install.log" -no-reboot </dev/null >>qemu-stderr.log 2>&1
    echo "node $n install: QEMU exited after $(( $(date +%s) - start )) s"
    "$LAB/show.sh" "rb$n.install.log" | grep -a 'reliaburger' | cut -c1-200
    ;;
run)
    creds=(-smbios "type=11,value=io.systemd.credential:ssh.authorized_keys.root=$(cat l2lab_key.pub)")
    seed="$WORK/cluster/seeds/node-$(printf %02d "$n").tgz"
    [ -f "$seed" ] && creds+=(-smbios "type=11,value=io.systemd.credential.binary:reliaburger.seed=$(base64 < "$seed" | tr -d '\n')")
    : > "rb$n.serial.log"
    # STICK=<image> plugs in a seed stick (make-seed-stick.sh) as USB storage.
    [ -n "${STICK:-}" ] && creds+=(-device qemu-xhci -device usb-storage,drive=stick,removable=on -drive "if=none,id=stick,format=raw,file=$STICK")
    qemu-system-aarch64 -name "rb$n" "${common[@]}" "${creds[@]}" -serial file:"rb$n.serial.log" \
        -pidfile "rb$n.pid" </dev/null >>qemu-stderr.log 2>&1 &
    sleep 1
    echo "node $n running on hub slot $slot, pid $(cat "rb$n.pid"), seed: $([ -f "$seed" ] && echo yes || echo no), log work/rb$n.serial.log"
    ;;
*) echo "install or run" >&2; exit 2 ;;
esac
