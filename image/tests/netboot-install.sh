#!/bin/bash
# The netboot install test (x86-64, KVM), run by .github/workflows/appliance.yml:
# a blank 8 GB disk and 2 GiB, QEMU's user network playing DHCP and TFTP (iPXE
# and boot.ipxe), a web server on the runner serving the installer and the
# signed image. The installer checks the signature, streams the image to disk
# and reboots; the disk comes first in the boot order, so the second boot
# starts the installed system, and the test passes once bun is healthy.
#
#   netboot-install.sh <artefact dir> <version>
#
# <artefact dir> holds what the build produced (and, for a published build,
# what the sign job signed): reliaburger-os_<version>.{raw.zst,SHA256SUMS,
# SHA256SUMS.sig}, the installer UKI, and netboot/. Needs qemu-system-x86,
# ovmf and a usable /dev/kvm.
set -euo pipefail
out=$(cd "${1:?usage: netboot-install.sh <artefact dir> <version>}" && pwd)
version=${2:?usage: netboot-install.sh <artefact dir> <version>}
work=$(mktemp -d)
web="$work/web" tftp="$work/tftp"
mkdir -p "$web/x86_64" "$tftp"
cp "$out/netboot/ipxe-snp-x86_64.efi" "$out/netboot/boot.ipxe" "$tftp/"
for f in "$out"/reliaburger-os_"$version".{raw.zst,SHA256SUMS,SHA256SUMS.sig}; do
    ln -s "$f" "$web/x86_64/"
done
ln -s "$out/reliaburger-os-installer_$version.efi" "$web/x86_64/installer.efi"
(cd "$web" && exec python3 -m http.server 8080 >"$work/http.log" 2>&1) &
http=$!
truncate -s 8G "$work/blank.raw"
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
log="$work/netboot.log"
qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$work/vars.fd" \
    -drive if=none,id=d0,format=raw,file="$work/blank.raw" \
    -device virtio-blk-pci,drive=d0,bootindex=1 \
    -netdev user,id=n0,tftp="$tftp",bootfile=ipxe-snp-x86_64.efi \
    -device virtio-net-pci,netdev=n0,bootindex=2 \
    -display none -serial "file:$log" &
qemu=$!
start=$(date +%s)
result=timeout
while [ $(( $(date +%s) - start )) -lt 900 ]; do
    if grep -q "reliaburger: bun healthy" "$log" 2>/dev/null; then result=pass; break; fi
    if grep -q -e "reliaburger-install: FAILED" -e "reliaburger: bun not healthy" "$log" 2>/dev/null; then result=fail; break; fi
    if ! kill -0 "$qemu" 2>/dev/null; then result=exited; break; fi
    sleep 5
done
elapsed=$(( $(date +%s) - start ))
kill "$qemu" "$http" 2>/dev/null || true
echo "--- serial console ---"
cat "$log" || true
echo "--- web server ---"
cat "$work/http.log" || true
{
    echo "### Netboot install test: $result after ${elapsed} s"
    echo '```'
    grep -a -e "reliaburger" "$log" | grep -v "reliaburger: journal:" || true
    echo '```'
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
[ "$result" = pass ]
